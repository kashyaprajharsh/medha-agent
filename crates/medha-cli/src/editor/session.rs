//! One editor attachment. Streaming continues while prompt admission, control
//! and approval RPCs are waiting. Only one ACP prompt may be outstanding for a
//! session; shared Desktop/TUI steering remains owned by the backend.
use super::{Ready, content, text, update};
use crate::chat::Writer;
use medha_client::{Said, View};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    task::JoinSet,
};

pub(super) struct Request {
    pub frame: Value,
    pub permit: Option<OwnedSemaphorePermit>,
    pub generation: u64,
}

pub(super) struct Handle {
    pub requests: mpsc::Sender<Request>,
    pub answers: mpsc::Sender<Request>,
    pub connection: Arc<medha_client::Connection>,
    pub chat: medha_client::Chat,
    pub generation: Arc<AtomicU64>,
}

struct Prompt {
    id: Value,
    turn: Option<u64>,
    generation: u64,
    awaiting_admission: bool,
    _permit: Option<OwnedSemaphorePermit>,
}
enum Job {
    Prepared(Result<protocol::SendMessage, String>),
    Sent(Result<protocol::MessageAccepted, String>),
    Control(Option<Value>, Result<Value, String>),
}

enum Permission {
    Approval {
        gate: u64,
        choices: HashMap<String, protocol::ApprovalDecision>,
    },
    Question {
        question: u64,
    },
}
pub(super) fn permission_session(id: &str) -> Option<&str> {
    id.strip_prefix("medha|")?.split('|').next()
}

pub(super) fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read" | "ls" | "skill" => "read",
        "write" | "edit" => "edit",
        "grep" | "glob" | "code" | "sessions.search" => "search",
        "shell.exec" | "git" | "diagnostics" => "execute",
        "web" => "fetch",
        "update_plan" | "clarify" => "think",
        _ => "other",
    }
}

fn approval(
    writer: &Writer,
    session: &str,
    stream: &str,
    prompt: &protocol::ApprovalPrompt,
    permissions: &mut HashMap<String, Permission>,
) {
    let id = format!("medha|{session}|{stream}|{}", prompt.gate_id);
    if permissions.contains_key(&id) {
        return;
    }
    let mut choices = HashMap::new();
    let options: Vec<_> = prompt
        .choices
        .iter()
        .enumerate()
        .filter_map(|(index, choice)| {
            use protocol::ApprovalDecision as D;
            let (name, kind) = match choice {
                D::Approve | D::Once => ("Allow once".to_owned(), "allow_once"),
                D::Always => {
                    let name = if let Some(path) = &prompt.path {
                        let access = match path.access {
                            protocol::PathAccess::Read => "read",
                            protocol::PathAccess::Write => "read/write",
                        };
                        let scope = match path.kind {
                            protocol::PathKind::File => "this file",
                            protocol::PathKind::Directory => "this folder and its contents",
                            protocol::PathKind::Unknown => "this path",
                        };
                        format!("Always allow {access} access to {scope}: {} (this project, including commands)", path.path)
                    } else if matches!(prompt.kind, protocol::ApprovalKind::Path) {
                        // Do not offer a durable grant without its resolved target.
                        return None;
                    } else {
                        "Allow this action for this chat".to_owned()
                    };
                    (name, "allow_always")
                }
                D::Session => ("Allow for this chat".to_owned(), "allow_always"),
                D::Persistent => ("Always allow for this project".to_owned(), "allow_always"),
                D::Folder => {
                    let path = prompt.path.as_ref()?;
                    let folder = prompt.folder.as_ref()?;
                    let access = match path.access {
                        protocol::PathAccess::Read => "read",
                        protocol::PathAccess::Write => "read/write",
                    };
                    (format!("Always allow {access} access to this folder and its contents: {folder} (this project, including commands)"), "allow_always")
                }
                D::Deny => ("Deny".to_owned(), "reject_once"),
            };
            let option = format!("choice-{index}");
            choices.insert(option.clone(), *choice);
            Some(json!({"optionId":option,"name":name,"kind":kind}))
        })
        .collect();
    writer.write_value(&json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission",
        "params":{"sessionId":session,"toolCall":{"toolCallId":id,
            "title":prompt.action,"kind":tool_kind(&prompt.action),"status":"pending",
            "rawInput":{"detail":prompt.detail,"path":prompt.path,"folder":prompt.folder}},"options":options}}));
    permissions.insert(
        id,
        Permission::Approval {
            gate: prompt.gate_id,
            choices,
        },
    );
}

fn question(
    writer: &Writer,
    session: &str,
    stream: &str,
    question: &protocol::QuestionPrompt,
    permissions: &mut HashMap<String, Permission>,
) {
    let id = format!("medha|{session}|{stream}|q{}", question.question_id);
    if permissions.contains_key(&id) {
        return;
    }
    writer.notify(
        "_medha/question",
        json!({"sessionId":session,"question":question}),
    );
    writer.write_value(
        &json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission",
        "params":{"sessionId":session,"toolCall":{"toolCallId":id,"title":"Medha needs an answer",
            "kind":"think","status":"pending","rawInput":question},"options":[
            {"optionId":"another-viewer","name":"Answer in Desktop or TUI","kind":"allow_once"},
            {"optionId":"dismiss","name":"Dismiss these questions","kind":"reject_once"}]}}),
    );
    permissions.insert(
        id,
        Permission::Question {
            question: question.question_id,
        },
    );
}

pub(super) async fn emit(writer: &Writer, session: &str, event: protocol::TurnEvent) {
    use protocol::TurnEvent as E;
    match event {
        E::Text { delta } => text(writer, session, "agent_message_chunk", &delta).await,
        E::Reasoning { delta } => text(writer, session, "agent_thought_chunk", &delta).await,
        E::User { content, .. } | E::Steered { content } => {
            text(writer, session, "user_message_chunk", &content).await
        }
        E::ToolCall { id, tool, args } => {
            update(
                writer,
                session,
                json!({"sessionUpdate":"tool_call","toolCallId":id.unwrap_or_else(|| tool.clone()),
                "title":tool,"kind":tool_kind(&tool),"status":"in_progress","rawInput":args}),
            );
        }
        E::ToolResult {
            id,
            tool,
            ok,
            payload,
        } => {
            let id = id.unwrap_or_else(|| tool.clone());
            let output = serde_json::to_string(&payload).expect("tool data serializes");
            // Large results use multiple content updates, never an oversized
            // JSON-RPC envelope or an unexplained truncation.
            let mut rest = output.as_str();
            while !rest.is_empty() {
                let mut end = rest.len().min(64 * 1024);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                let last = end == rest.len();
                writer.wait_for_room().await;
                update(
                    writer,
                    session,
                    json!({"sessionUpdate":"tool_call_update","toolCallId":id,
                    "status":if last { if ok {"completed"} else {"failed"} } else {"in_progress"},
                    "content":[{"type":"content","content":{"type":"text","text":&rest[..end]}}]}),
                );
                rest = &rest[end..];
            }
        }
        E::Notice { text: notice } => {
            text(
                writer,
                session,
                "agent_message_chunk",
                &format!("\n{notice}\n"),
            )
            .await
        }
        E::Error { message } => {
            text(
                writer,
                session,
                "agent_message_chunk",
                &format!("\nError: {message}\n"),
            )
            .await
        }
        E::Returned { contents } => {
            text(
                writer,
                session,
                "agent_message_chunk",
                &format!("\nUnsent follow-ups: {}\n", contents.join("\n")),
            )
            .await
        }
        E::Verify { ok, summary } => {
            text(
                writer,
                session,
                "agent_message_chunk",
                &format!(
                    "\nVerification {}: {summary}\n",
                    if ok { "passed" } else { "failed" }
                ),
            )
            .await
        }
        E::Restarted => {
            text(
                writer,
                session,
                "agent_message_chunk",
                "\nMedha is retrying the interrupted model attempt.\n",
            )
            .await
        }
        _ => {}
    }
}

async fn deliver(writer: &Writer, session: &str, event: protocol::TurnEvent, view: &View) {
    let images = match &event {
        protocol::TurnEvent::User { images, .. } => images.clone(),
        _ => Vec::new(),
    };
    emit(writer, session, event).await;
    replay_images(writer, session, &images, view).await;
}

async fn admitted_users(
    writer: &Writer,
    session: &str,
    users: &mut Vec<protocol::TurnEvent>,
    own_turn: Option<u64>,
    view: &View,
) {
    for event in users.drain(..) {
        if matches!(&event, protocol::TurnEvent::User { turn, .. } if Some(*turn) == own_turn) {
            continue;
        }
        deliver(writer, session, event, view).await;
    }
}

async fn replay_images(
    writer: &Writer,
    session: &str,
    images: &[protocol::SavedImage],
    view: &View,
) {
    for image in images {
        match view
            .call(&protocol::ReadHistoryImage {
                hash: image.hash.clone(),
            })
            .await
        {
            Ok(saved) => {
                writer.wait_for_room().await;
                if let Some(note) = saved.note {
                    text(
                        writer,
                        session,
                        "user_message_chunk",
                        &format!("\n{}: {note}\n", image.name),
                    )
                    .await;
                }
                update(
                    writer,
                    session,
                    json!({"sessionUpdate":"user_message_chunk",
                    "content":{"type":"image","mimeType":saved.mime,"data":saved.data}}),
                );
            }
            Err(error) => {
                text(
                    writer,
                    session,
                    "user_message_chunk",
                    &format!("\n[image unavailable: {} — {error}]\n", image.name),
                )
                .await
            }
        }
    }
}

pub(super) async fn presentation(
    writer: &Writer,
    session: &str,
    item: protocol::PresentationItem,
    view: &View,
) {
    use protocol::{PresentationItem as I, TurnEvent as E};
    match item {
        I::User {
            text: content,
            images,
        } => {
            text(writer, session, "user_message_chunk", &content).await;
            replay_images(writer, session, &images, view).await;
        }
        I::Assistant { text: content } => {
            text(writer, session, "agent_message_chunk", &content).await
        }
        I::Reasoning { text: content } => {
            text(writer, session, "agent_thought_chunk", &content).await
        }
        I::ToolCall { id, tool, args } => {
            emit(writer, session, E::ToolCall { id, tool, args }).await
        }
        I::ToolResult {
            id,
            tool,
            ok,
            payload,
        } => {
            emit(
                writer,
                session,
                E::ToolResult {
                    id,
                    tool,
                    ok,
                    payload,
                },
            )
            .await
        }
        I::Notice { text: content } => emit(writer, session, E::Notice { text: content }).await,
        I::Verify { ok, summary } => emit(writer, session, E::Verify { ok, summary }).await,
        I::Compaction {
            summary: Some(summary),
            ..
        } => {
            text(
                writer,
                session,
                "agent_message_chunk",
                &format!("\nCompaction summary: {summary}\n"),
            )
            .await
        }
        _ => {}
    }
}

fn complete(writer: &Writer, prompt: &mut Option<Prompt>, completion: &protocol::TurnCompletion) {
    let Some(active) = prompt.as_ref() else {
        return;
    };
    if active.turn != Some(completion.turn) {
        return;
    }
    let active = prompt.take().expect("pending prompt");
    use protocol::TurnOutcome as O;
    let reason = match &completion.outcome {
        O::Finished => "end_turn",
        O::Cancelled => "cancelled",
        O::Refused => "refusal",
        O::TokenLimit => "max_tokens",
        O::RequestLimit => "max_turn_requests",
        O::Failed { message } => {
            writer.error(active.id, -32000, message);
            return;
        }
    };
    writer.respond(active.id, json!({"stopReason":reason}));
}

fn control<T: protocol::Command + Send + Sync + 'static>(
    jobs: &mut JoinSet<Job>,
    view: &View,
    id: Option<Value>,
    command: T,
    permit: Option<OwnedSemaphorePermit>,
) where
    T::Output: serde::Serialize,
{
    let connection = view.connection();
    let chat = view.chat();
    jobs.spawn(async move {
        let _permit = permit;
        Job::Control(
            id,
            connection
                .call_chat(&chat, &command)
                .await
                .map(|_| json!({})),
        )
    });
}

fn query<T: protocol::Command + Send + Sync + 'static>(
    jobs: &mut JoinSet<Job>,
    view: &View,
    id: Value,
    command: T,
    permit: Option<OwnedSemaphorePermit>,
) where
    T::Output: serde::Serialize,
{
    let connection = view.connection();
    let chat = view.chat();
    jobs.spawn(async move {
        let _permit = permit;
        Job::Control(
            Some(id),
            connection
                .call_chat(&chat, &command)
                .await
                .map(|value| json!(value)),
        )
    });
}

pub(super) async fn run(
    ready: Ready,
    mut requests: mpsc::Receiver<Request>,
    mut answers: mpsc::Receiver<Request>,
    generation: Arc<AtomicU64>,
    writer: Arc<Writer>,
) {
    let Ready {
        mut view,
        session,
        snapshot,
        reply,
    } = ready;
    let stream = view
        .cursor()
        .map(|cursor| cursor.stream)
        .unwrap_or_default();
    let mut permissions = HashMap::new();
    let mut prompt: Option<Prompt> = None;
    let mut completions = BTreeMap::new();
    let mut jobs = JoinSet::new();
    let mut users = Vec::new();
    let mut user_bytes = 0usize;
    for gate in &snapshot.approvals {
        approval(&writer, &session, &stream, gate, &mut permissions);
    }
    for form in &snapshot.questions {
        question(&writer, &session, &stream, form, &mut permissions);
    }
    let mut result = json!({"sessionId":session});
    if let Some(settings) = snapshot.settings {
        result["modes"] = json!({"currentModeId":settings.mode,"availableModes":[
            {"id":"plan","name":"Plan"},{"id":"careful","name":"Careful"},
            {"id":"normal","name":"Normal"},{"id":"yolo","name":"Yolo"}]});
    }
    writer.respond(reply, result);
    loop {
        tokio::select! {
            said = view.recv() => {
                let frame = match said {
                    Some(Said::Frame(frame) | Said::Event { frame, .. }) => frame,
                    Some(Said::Ended(reason)) => {
                        if let Some(prompt) = prompt.take() { writer.error(prompt.id, -32000, reason.unwrap_or_else(|| "The backend chat ended".into())); }
                        break;
                    }
                    None => break,
                };
                match frame["method"].as_str() {
                    Some("event") => if let Ok(event) = serde_json::from_value::<protocol::TurnEvent>(frame["params"].clone()) {
                        if let protocol::TurnEvent::Settled { completion } = &event {
                            completions.insert(completion.turn, completion.clone());
                            while completions.len() > 64 { completions.pop_first(); }
                            complete(&writer, &mut prompt, completion);
                        }
                        if let protocol::TurnEvent::User { turn, .. } = &event
                            && let Some(active) = prompt.as_ref() {
                            // An editor has already drawn the prompt it sent.
                            // Admission may arrive after its user event; defer
                            // that event until its exact turn identity is known.
                            if active.turn == Some(*turn) { continue; }
                            if active.awaiting_admission && active.turn.is_none() {
                                let size = serde_json::to_vec(&event).expect("typed event serializes").len();
                                if users.len() >= 64 || user_bytes.saturating_add(size) > 16*1024*1024 {
                                    if let Some(active) = prompt.take() { writer.error(active.id, -32002, "Editor admission events exceeded the viewer budget. Reload the session."); }
                                    break;
                                }
                                user_bytes += size; users.push(event); continue;
                            }
                        }
                        if !users.is_empty() {
                            let size = serde_json::to_vec(&event).expect("typed event serializes").len();
                            if users.len() >= 64 || user_bytes.saturating_add(size) > 16*1024*1024 {
                                if let Some(active) = prompt.take() { writer.error(active.id, -32002, "Editor admission events exceeded the viewer budget. Reload the session."); }
                                break;
                            }
                            user_bytes += size; users.push(event); continue;
                        }
                        deliver(&writer, &session, event, &view).await;
                    },
                    Some("approval") => if let Ok(gate) = serde_json::from_value(frame["params"].clone()) {
                        approval(&writer, &session, &stream, &gate, &mut permissions);
                    },
                    Some("approval.resolved") => if let Some(gate) = frame["params"]["gate_id"].as_u64() {
                        permissions.retain(|_, pending| !matches!(pending, Permission::Approval { gate: waiting, .. } if *waiting == gate));
                    },
                    Some("question") => {
                        if let Ok(form) = serde_json::from_value::<protocol::QuestionPrompt>(frame["params"].clone()) {
                            text(&writer, &session, "agent_message_chunk", &format!("\nMedha needs an answer. Open this chat in Desktop/TUI to answer, or cancel the turn.\n{}\n",
                                form.questions.iter().map(|question| question.prompt.as_str()).collect::<Vec<_>>().join("\n"))).await;
                            question(&writer, &session, &stream, &form, &mut permissions);
                        }
                    }
                    Some("question.answered") => if let Some(question) = frame["params"]["question_id"].as_u64() {
                        permissions.retain(|_, pending| !matches!(pending, Permission::Question { question: waiting } if *waiting == question));
                    },
                    Some("settings") => if let Some(mode) = frame["params"].get("mode") {
                        update(&writer, &session, json!({"sessionUpdate":"current_mode_update","currentModeId":mode}));
                    },
                    _ => {}
                }
            }
            Some(job) = jobs.join_next(), if !jobs.is_empty() => match job {
                Ok(Job::Prepared(result)) => {
                    if let Some(active) = prompt.as_ref() {
                        if active.generation != generation.load(Ordering::Acquire) {
                            let active = prompt.take().unwrap(); writer.respond(active.id, json!({"stopReason":"cancelled"}));
                        } else { match result {
                            Ok(command) => { prompt.as_mut().expect("prepared prompt").awaiting_admission = true;
                                let connection = view.connection(); let chat = view.chat();
                                jobs.spawn(async move { Job::Sent(connection.call_chat(&chat, &command).await) }); }
                            Err(error) => { let active = prompt.take().unwrap(); writer.error(active.id, -32602, error); }
                        }}
                    }
                }
                Ok(Job::Sent(result)) => match result {
                    Ok(accepted) => {
                        if let Some(active) = prompt.as_mut() {
                            active.turn = Some(accepted.turn);
                            if active.generation != generation.load(Ordering::Acquire) { control(&mut jobs, &view, None, protocol::CancelTurn { turn:accepted.turn }, None); }
                        }
                        admitted_users(&writer, &session, &mut users, Some(accepted.turn), &view).await;
                        user_bytes = 0;
                        if let Some(completion) = completions.get(&accepted.turn) { complete(&writer, &mut prompt, completion); }
                    },
                    Err(error) => {
                        admitted_users(&writer, &session, &mut users, None, &view).await;
                        user_bytes = 0;
                        if let Some(active) = prompt.take() { writer.error(active.id, -32002, error); }
                    },
                },
                Ok(Job::Control(id, result)) => if let Some(id) = id {
                    match result { Ok(value) => { writer.respond(id, value); }, Err(error) => { writer.error(id, -32001, error); } }
                },
                Err(error) => { if let Some(active) = prompt.take() { writer.error(active.id, -32000, format!("Editor request failed: {error}")); } break; }
            },
            request = async { tokio::select! { biased; answer = answers.recv() => answer, request = requests.recv() => request } } => {
                let Some(Request { frame, permit, generation }) = request else { break; };
                let id = frame.get("id").cloned();
                if frame.get("method").is_none() {
                    let Some(permission) = id.as_ref().and_then(Value::as_str).and_then(|id| permissions.remove(id)) else { continue; };
                    let chosen = (frame["result"]["outcome"]["outcome"] == "selected")
                        .then(|| frame["result"]["outcome"]["optionId"].as_str()).flatten();
                    match permission {
                        Permission::Approval { gate, choices } => {
                            let decision = chosen.and_then(|id| choices.get(id)).copied().unwrap_or(protocol::ApprovalDecision::Deny);
                            control(&mut jobs, &view, None, protocol::AnswerApproval { gate_id: gate, decision }, None);
                        }
                        Permission::Question { question } => if chosen != Some("another-viewer") {
                            control(&mut jobs, &view, None, protocol::AnswerQuestion { question_id: question, dismiss: true, answers: Vec::new() }, None);
                        },
                    }
                    continue;
                }
                let method = frame["method"].as_str().unwrap_or("");
                if jobs.len() >= 16 { if let Some(id) = id { writer.error(id, -32002, "Too many session requests"); } continue; }
                match method {
                    "session/prompt" => if let Some(id) = id {
                        if prompt.is_some() { writer.error(id, -32002, "Wait for the outstanding prompt or cancel it"); continue; }
                        prompt = Some(Prompt { id, turn: None, generation, awaiting_admission:false, _permit: permit });
                        let blocks = frame["params"]["prompt"].clone();
                        jobs.spawn(async move { Job::Prepared(content::prompt(blocks).await) });
                    },
                    "session/set_mode" => if let Some(id) = id {
                        match serde_json::from_value::<protocol::Mode>(frame["params"]["modeId"].clone()) {
                            Ok(mode) => control(&mut jobs, &view, Some(id), protocol::Configure::Mode(mode), permit),
                            Err(_) => { writer.error(id, -32602, "Unknown modeId"); }
                        }
                    },
                    "_medha/question.respond" => if let Some(id) = id {
                        let mut answer = frame["params"].clone();
                        if let Some(object) = answer.as_object_mut() { object.remove("sessionId"); }
                        match serde_json::from_value::<protocol::AnswerQuestion>(answer) {
                            Ok(answer) => control(&mut jobs, &view, Some(id), answer, permit),
                            Err(_) => { writer.error(id, -32602, "Invalid question answer"); }
                        }
                    },
                    "_medha/session.settings" => if let Some(id) = id { query(&mut jobs, &view, id, protocol::GetSettings {}, permit); },
                    "_medha/session.configure" => if let Some(id) = id {
                        match serde_json::from_value::<protocol::Configure>(frame["params"]["change"].clone()) {
                            Ok(change) => query(&mut jobs, &view, id, change, permit),
                            Err(_) => { writer.error(id, -32602, "Invalid configuration change"); }
                        }
                    },
                    _ => if let Some(id) = id { writer.error(id, -32601, "Method not supported"); },
                }
            }
            _ = writer.cancelled() => break,
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    // View's drop detaches only this editor. A chat watched by another
    // frontend survives; a chat with no viewers follows backend lifetime policy.
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    async fn offered(prompt: protocol::ApprovalPrompt) -> Value {
        let (output, input) = tokio::io::duplex(4096);
        let (writer, task) = crate::chat::output_writer(output, 8);
        approval(&writer, "session", "stream", &prompt, &mut HashMap::new());
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::io::BufReader::new(input).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        task.finish(&writer).await;
        serde_json::from_str(&line).unwrap()
    }

    fn path_prompt(
        kind: protocol::PathKind,
        access: protocol::PathAccess,
    ) -> protocol::ApprovalPrompt {
        protocol::ApprovalPrompt {
            gate_id: 7,
            action: "Read outside workspace".into(),
            detail: None,
            escalated: false,
            kind: protocol::ApprovalKind::Path,
            choices: vec![
                protocol::ApprovalDecision::Once,
                protocol::ApprovalDecision::Always,
                protocol::ApprovalDecision::Folder,
                protocol::ApprovalDecision::Deny,
            ],
            path: Some(protocol::ApprovalPath {
                path: "/projects/other folder/notes.txt".into(),
                kind,
                access,
            }),
            folder: Some("/projects/other folder".into()),
        }
    }

    #[tokio::test]
    async fn remembered_path_options_identify_persistence_target_access_and_commands() {
        for (kind, access, scope, permission) in [
            (
                protocol::PathKind::File,
                protocol::PathAccess::Read,
                "this file",
                "read",
            ),
            (
                protocol::PathKind::Directory,
                protocol::PathAccess::Read,
                "this folder and its contents",
                "read",
            ),
            (
                protocol::PathKind::Unknown,
                protocol::PathAccess::Write,
                "this path",
                "read/write",
            ),
        ] {
            let frame = offered(path_prompt(kind, access)).await;
            let options = &frame["params"]["options"];
            assert_eq!(options[0]["name"], "Allow once");
            assert_eq!(options[0]["kind"], "allow_once");
            assert_eq!(
                options[1]["name"],
                format!(
                    "Always allow {permission} access to {scope}: /projects/other folder/notes.txt (this project, including commands)"
                )
            );
            assert_eq!(options[1]["kind"], "allow_always");
            assert_eq!(
                options[2]["name"],
                format!(
                    "Always allow {permission} access to this folder and its contents: /projects/other folder (this project, including commands)"
                )
            );
            assert_eq!(options[2]["kind"], "allow_always");
            assert_eq!(options[3]["kind"], "reject_once");
        }
    }

    #[tokio::test]
    async fn action_options_keep_their_session_scope_and_persistent_access_is_explicit() {
        let mut prompt = path_prompt(protocol::PathKind::File, protocol::PathAccess::Read);
        prompt.kind = protocol::ApprovalKind::Action;
        prompt.path = None;
        prompt.folder = None;
        prompt.choices = vec![
            protocol::ApprovalDecision::Always,
            protocol::ApprovalDecision::Session,
            protocol::ApprovalDecision::Persistent,
        ];
        let frame = offered(prompt).await;
        let options = &frame["params"]["options"];
        assert_eq!(options[0]["name"], "Allow this action for this chat");
        assert_eq!(options[1]["name"], "Allow for this chat");
        assert_eq!(options[2]["name"], "Always allow for this project");
    }

    #[tokio::test]
    async fn a_missing_path_or_folder_cannot_offer_an_unidentified_persistent_grant() {
        let mut prompt = path_prompt(protocol::PathKind::File, protocol::PathAccess::Read);
        prompt.path = None;
        prompt.folder = None;
        let frame = offered(prompt).await;
        assert_eq!(
            frame["params"]["options"],
            json!([
                {"optionId":"choice-0","name":"Allow once","kind":"allow_once"},
                {"optionId":"choice-3","name":"Deny","kind":"reject_once"}
            ])
        );
    }
}
