//! ACP is an editor adapter over medha-client. It owns attachments and protocol
//! translation, never a runtime, model provider, event log or MCP host.
mod content;
mod history;
mod session;

use crate::chat::Writer;
use medha_client::{Backend, View, ViewLimits};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, BufReader},
    sync::{Semaphore, mpsc},
    task::JoinSet,
};

const MAX_SESSIONS: usize = 32;
const INPUT_BYTES: usize = 32 * 1024 * 1024;
const REQUIRED: &[&str] = &[
    "editor-sessions",
    "presentation-snapshot",
    "live-conversation",
    "priority-controls",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Open {
    cwd: PathBuf,
    #[serde(default)]
    session_id: Option<String>,
    mcp_servers: Vec<StdioServer>,
    #[serde(default)]
    additional_directories: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct StdioServer {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    env: Vec<Env>,
    #[serde(rename = "type")]
    transport: Option<String>,
}
#[derive(Deserialize)]
struct Env {
    name: String,
    value: String,
}

impl Open {
    fn servers(&self) -> Result<Vec<protocol::SessionMcpServer>, String> {
        let mut servers = Vec::new();
        for server in &self.mcp_servers {
            if server
                .transport
                .as_deref()
                .is_some_and(|kind| kind != "stdio")
            {
                return Err("This editor adapter supports stdio MCP servers.".into());
            }
            let mut env = BTreeMap::new();
            for entry in &server.env {
                if env
                    .insert(entry.name.clone(), entry.value.clone())
                    .is_some()
                {
                    return Err("Duplicate MCP environment name.".into());
                }
            }
            servers.push(protocol::SessionMcpServer {
                name: server.name.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                env,
            });
        }
        runtime::session_mcp::validate(&servers).map_err(|e| e.to_string())?;
        Ok(servers)
    }
}

pub(super) struct Ready {
    view: View,
    session: String,
    snapshot: protocol::PresentationSnapshot,
    reply: Value,
}

async fn open(
    backend: Arc<Backend>,
    mut startup: protocol::StartupOptions,
    params: Open,
    method: String,
    reply: Value,
    writer: Arc<Writer>,
) -> Result<Ready, String> {
    if !params.cwd.is_absolute() {
        return Err("cwd must be an absolute path.".into());
    }
    if !params.additional_directories.is_empty() {
        return Err("Additional workspace roots are not supported by this adapter.".into());
    }
    startup.session_mcp = params.servers()?;
    let folder = tokio::task::spawn_blocking(move || params.cwd.canonicalize())
        .await
        .map_err(|_| "Workspace lookup stopped")?
        .map_err(|e| e.to_string())?;
    let identity = runtime::session_mcp::identity(&startup.session_mcp);
    let connection = tokio::task::spawn_blocking(move || backend.connection())
        .await
        .map_err(|_| "Backend connection stopped")??;
    let requested = if method == "session/new" {
        // CLI --resume/--continue remain useful to launch editors into the
        // caller's chosen conversation. Normal session/new starts fresh.
        match &startup.resume {
            protocol::Resume::Id(id) => Some(id.clone()),
            protocol::Resume::Latest => {
                let sessions = connection
                    .call_async(
                        protocol::Scope::Folder(&folder),
                        &protocol::ReadResource::Sessions,
                    )
                    .await?;
                match sessions {
                    protocol::ResourceResult::Sessions(sessions) => {
                        sessions.first().map(|session| session.id.clone())
                    }
                    _ => return Err("The backend returned invalid session metadata".into()),
                }
            }
            protocol::Resume::None => None,
        }
    } else {
        Some(params.session_id.ok_or("sessionId is required")?)
    };
    if requested.is_none() {
        startup.resume = protocol::Resume::None;
    }
    let listed = connection
        .call_async(protocol::Scope::Service, &protocol::ListSessions {})
        .await?;
    let live = requested.as_ref().and_then(|id| {
        listed
            .sessions
            .iter()
            .find(|live| live.session == *id || live.conversation.as_ref() == Some(id))
    });
    let (mut view, session) = if let Some(live) = live {
        let about = live
            .about
            .as_ref()
            .ok_or("The live chat has no workspace identity")?;
        if about.folder != folder {
            return Err("This conversation belongs to another workspace.".into());
        }
        if identity.is_some() && identity != about.session_mcp {
            return Err(
                "The live chat has different session MCP servers. Finish it before changing them."
                    .into(),
            );
        }
        let (view, _) = View::attach(
            connection.clone(),
            live.session.clone(),
            None,
            ViewLimits::default(),
        )
        .await?;
        (view, live.session.clone())
    } else {
        if let Some(id) = &requested {
            startup.resume = protocol::Resume::Id(id.clone());
        }
        let (view, made, _) = View::create(
            connection.clone(),
            &protocol::CreateSession {
                folder,
                ends_with_client: true,
                resume: requested.clone(),
                model: None,
                mode: None,
                reasoning: None,
                settings: None,
                startup: Some(startup),
            },
            ViewLimits::default(),
        )
        .await?;
        (view, made.session)
    };
    let snapshot = if method == "session/load" || (method == "session/new" && requested.is_some()) {
        history::replay(&mut view, &writer, &session).await?
    } else {
        let snapshot: protocol::PresentationSnapshot =
            view.call(&protocol::GetPresentation {}).await?;
        snapshot
    };
    Ok(Ready {
        view,
        session,
        snapshot,
        reply,
    })
}

pub(crate) fn update(writer: &Writer, session: &str, value: Value) -> bool {
    writer.notify(
        "session/update",
        json!({"sessionId":session,"update":value}),
    )
}

pub(crate) async fn text(writer: &Writer, session: &str, kind: &str, content: &str) {
    // Text chunks are byte-bounded even when replaying a single enormous event.
    let mut rest = content;
    while !rest.is_empty() {
        let mut end = rest.len().min(64 * 1024);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        writer.wait_for_room().await;
        if !update(
            writer,
            session,
            json!({"sessionUpdate":kind,"content":{"type":"text","text":&rest[..end]}}),
        ) {
            break;
        }
        rest = &rest[end..];
    }
}

pub(crate) async fn run<R, W>(
    startup: protocol::StartupOptions,
    input: R,
    output: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let backend = Backend::for_surface(
        || std::env::current_exe().map_err(|e| e.to_string()),
        REQUIRED,
    );
    run_with(backend, startup, input, output).await
}

async fn run_with<R, W>(
    backend: Arc<Backend>,
    startup: protocol::StartupOptions,
    input: R,
    output: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (writer, output_task) = crate::chat::output_writer(output, 256);
    let mut input = BufReader::new(input).take(wire::MAX_FRAME as u64);
    let mut partial = Vec::new();
    let mut initialized = false;
    let mut sessions: HashMap<String, session::Handle> = HashMap::new();
    let mut opening = HashSet::new();
    let mut starts = JoinSet::new();
    let mut actors = JoinSet::new();
    let mut controls = JoinSet::new();
    let bytes = Arc::new(Semaphore::new(INPUT_BYTES));
    loop {
        tokio::select! {
            line = crate::chat::read_frame(&mut input, &mut partial) => {
                let Some(line) = line? else { break; };
                let frame = match serde_json::from_str::<Value>(&line) {
                    Ok(frame) => frame,
                    Err(_) => { writer.error(Value::Null, -32700, "Invalid JSON"); continue; }
                };
                let id = frame.get("id").cloned();
                if frame["jsonrpc"] != "2.0" || id.as_ref().is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64()) {
                    writer.error(id.unwrap_or(Value::Null), -32600, "Invalid JSON-RPC envelope"); continue;
                }
                // Permission responses use a session/stream-scoped request id.
                if frame.get("method").is_none() {
                    if let Some(session) = id.as_ref().and_then(Value::as_str).and_then(session::permission_session)
                        && let Some(actor) = sessions.get(session) {
                        if line.len() > wire::CONTROL_FRAME_BYTES { anyhow::bail!("An editor permission response exceeded the control limit"); }
                        if actor.answers.try_send(session::Request { frame, permit: None, generation: 0 }).is_err() {
                            anyhow::bail!("The editor sent too many approval answers");
                        }
                    }
                    continue;
                }
                let method = frame["method"].as_str().unwrap_or("");
                if method == "initialize" {
                    if initialized { if let Some(id) = id { writer.error(id, -32600, "Already initialized"); } continue; }
                    if !frame["params"]["protocolVersion"].is_u64() {
                        if let Some(id) = id { writer.error(id, -32602, "protocolVersion is required"); } continue;
                    }
                    initialized = true;
                    if let Some(id) = id { writer.respond(id, json!({"protocolVersion":1,
                        "agentInfo":{"name":"medha","version":env!("CARGO_PKG_VERSION")},
                        "agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}},
                            "promptCapabilities":{"image":true,"audio":false,"embeddedContext":true}},
                        "authMethods":[]})); }
                    continue;
                }
                if !initialized { if let Some(id) = id { writer.error(id, -32000, "Initialize first"); } continue; }
                if method == "authenticate" {
                    if let Some(id) = id { writer.error(id, -32601, "No editor authentication method is advertised. Configure Medha credentials locally."); }
                    continue;
                }
                if matches!(method, "session/new" | "session/load" | "session/resume") {
                    let Some(id) = id else { continue; };
                    let params: Open = match serde_json::from_value(frame["params"].clone()) {
                        Ok(params) => params,
                        Err(_) => { writer.error(id, -32602, "Invalid session parameters"); continue; }
                    };
                    if sessions.len() + starts.len() >= MAX_SESSIONS || starts.len() >= 4 {
                        writer.error(id, -32002, "Too many editor sessions are opening"); continue;
                    }
                    if let Some(session) = &params.session_id
                        && (sessions.contains_key(session) || !opening.insert(session.clone())) {
                        writer.error(id, -32002, "This editor already follows that chat"); continue;
                    }
                    let requested = params.session_id.clone();
                    let backend = backend.clone(); let startup = startup.clone(); let writer = writer.clone();
                    let method = method.to_owned();
                    starts.spawn(async move {
                        let result = open(backend, startup, params, method, id.clone(), writer).await;
                        (id, requested, result)
                    });
                    continue;
                }
                let Some(session) = frame["params"]["sessionId"].as_str() else {
                    if let Some(id) = id { writer.error(id, -32602, "sessionId is required"); } continue;
                };
                let Some(actor) = sessions.get(session) else {
                    if let Some(id) = id { writer.error(id, -32602, "This editor does not follow that session"); } continue;
                };
                if method == "session/cancel" {
                    if controls.len() >= 64 {
                        if let Some(id) = id { writer.error(id, -32002, "Too many control requests"); }
                        continue;
                    }
                    actor.generation.fetch_add(1, Ordering::AcqRel);
                    let connection = actor.connection.clone(); let chat = actor.chat.clone();
                    controls.spawn(async move { (id, connection.call_chat(&chat, &protocol::Cancel {}).await) });
                    continue;
                }
                let permit = match bytes.clone().try_acquire_many_owned(line.len() as u32) {
                        Ok(permit) => Some(permit),
                        Err(_) => { if let Some(id) = id { writer.error(id, -32002, "Editor input queue is full"); } continue; }
                };
                if actor.requests.try_send(session::Request { frame, permit, generation: actor.generation.load(Ordering::Acquire) }).is_err()
                    && let Some(id) = id {
                    writer.error(id, -32002, "This session is busy or has ended");
                }
            }
            Some(opened) = starts.join_next(), if !starts.is_empty() => {
                match opened {
                    Ok((id, requested, result)) => {
                        if let Some(requested) = requested { opening.remove(&requested); }
                        match result {
                            Ok(ready) => {
                                let (tx, rx) = mpsc::channel(16);
                                let (answers, answered) = mpsc::channel(64);
                                let session = ready.session.clone();
                                if sessions.contains_key(&session) {
                                    writer.error(id, -32002, "This editor already follows that chat"); continue;
                                }
                                let generation = Arc::new(AtomicU64::new(0));
                                sessions.insert(session.clone(), session::Handle { requests: tx, answers,
                                    connection: ready.view.connection(), chat: ready.view.chat(), generation: generation.clone() });
                                let writer = writer.clone();
                                actors.spawn(async move { session::run(ready, rx, answered, generation, writer).await; session });
                            }
                            Err(error) => { writer.error(id, -32001, error); }
                        }
                    }
                    Err(error) => { anyhow::bail!("Editor session startup failed: {error}"); }
                }
            }
            Some(actor) = actors.join_next(), if !actors.is_empty() => {
                match actor { Ok(session) => { sessions.remove(&session); }, Err(error) => { anyhow::bail!("Editor attachment failed: {error}"); } }
            }
            Some(control) = controls.join_next(), if !controls.is_empty() => match control {
                Ok((Some(id), Ok(_))) => { writer.respond(id, json!({})); }
                Ok((Some(id), Err(error))) => { writer.error(id, -32001, error); }
                Ok((None, _)) => {}
                Err(error) => { anyhow::bail!("Editor control failed: {error}"); }
            },
            _ = writer.cancelled() => break,
        }
    }
    sessions.clear();
    starts.abort_all();
    actors.abort_all();
    controls.abort_all();
    while starts.join_next().await.is_some() {}
    while actors.join_next().await.is_some() {}
    while controls.join_next().await.is_some() {}
    output_task.finish(&writer).await;
    Ok(())
}

#[cfg(test)]
mod tests;
