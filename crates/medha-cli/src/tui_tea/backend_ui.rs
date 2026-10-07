//! The terminal is a viewer and command producer. Runtime, policy, credentials,
//! conversation ownership and tool execution belong to the shared backend.
pub(super) use super::backend_features::begin_model_setup;
pub(super) use super::backend_features::*;
use super::*;
use medha_client::{Backend, Chat, Connection, Said, View, ViewLimits};
use protocol::{ReadResource as Read, ResourceResult as Resource, SessionRead as Inspect};
use serde_json::Value;
use std::path::PathBuf;
use tokio::task::JoinSet;

const NORMAL_REQUESTS: usize = 2;
const CONTROL_REQUESTS: usize = 4;
const REQUEST_BYTES: usize = 32 * 1024 * 1024;

pub(super) struct Peer {
    backend: Arc<Backend>,
    connection: Arc<Connection>,
    chat: Chat,
    folder: PathBuf,
    startup: protocol::StartupOptions,
    generation: u64,
    pub(super) conversation: String,
    pub(super) turn: u64,
    pub(super) live: protocol::LiveState,
    pub(super) models: protocol::ModelCatalogue,
    pub(super) plugins: protocol::PluginCatalogue,
    pub(super) skills: Option<protocol::SkillCatalogue>,
    pub(super) mcp: Vec<protocol::McpDefinition>,
    pub(super) patches: Vec<protocol::AgentPatch>,
    pub(super) settings: Option<protocol::Settings>,
    pub(super) sending: bool,
    pub(super) agent_requests: usize,
    settled_turn: u64,
    pending_steers: Vec<String>,
    normal: usize,
    control: usize,
    bytes: usize,
    pending: VecDeque<Pending>,
    live_pending: bool,
    pub(super) recovering: bool,
    ended: bool,
    approvals_answering: std::collections::HashSet<u64>,
    questions_answering: std::collections::HashSet<u64>,
    questions: VecDeque<protocol::QuestionPrompt>,
}

struct Pending {
    effect: Effect,
    form: Option<u64>,
    command: Option<String>,
}

#[derive(Clone)]
pub(super) enum After {
    Startup,
    Notice,
    Models,
    Skills,
    Sources,
    Plugins,
    PluginsAt(String),
    PluginEnable(String),
    PluginDisable(String),
    PluginRemove(String),
    DiscoverPlugins,
    Mcp,
    Agents,
    Tasks,
    Memory,
    SkillList,
    SearchSaved,
    KeySaved(String),
    AgentTranscript(String),
    Patch(String),
    Reload(Box<After>),
}

pub(super) struct Draft {
    pub(super) raw: String,
    pub(super) request: protocol::SendMessage,
    pub(super) target: Option<String>,
    pub(super) followup: bool,
}

pub(super) struct DeferredSend {
    pub(super) raw: String,
    pub(super) intent: protocol::SendIntent,
    pub(super) target: Option<String>,
    pub(super) followup: bool,
}

pub(super) enum Effect {
    Read(Read, After),
    Change(protocol::ChangeResource, After),
    Inspect(Inspect, After),
    SessionChange(protocol::SessionChange, After),
    Configure(protocol::Configure),
    Reload(After),
    SavedProfile(protocol::ActivateSavedProfile),
    Mcp(protocol::McpCommand, After),
    Send(Draft),
    Expand(Draft),
    Approve(protocol::AnswerApproval),
    Question(protocol::AnswerQuestion),
    Cancel,
    Abort,
    Agent(protocol::AgentCommand, Option<String>),
    Discover(protocol::DiscoverModels),
    UseSkill(protocol::UseSkill),
    Rewind(protocol::Rewind),
    Points,
    Resume(String),
    Reconnect,
    Clear,
    Snapshot,
    Images {
        generation: u64,
        source: input::ImageSource,
    },
}

impl Effect {
    fn agent_request(&self) -> bool {
        matches!(
            self,
            Self::Agent(
                protocol::AgentCommand::Followup { .. } | protocol::AgentCommand::Steer { .. },
                _
            ) | Self::Send(Draft {
                target: Some(_),
                ..
            })
        )
    }
    fn control(&self) -> bool {
        matches!(
            self,
            Self::Approve(_)
                | Self::Question(_)
                | Self::Cancel
                | Self::Abort
                | Self::Snapshot
                | Self::Agent(protocol::AgentCommand::Stop { .. }, _)
                | Self::SessionChange(protocol::SessionChange::StopAgents, _)
        )
    }
    fn size(&self) -> usize {
        use crate::chat_presentation::size;
        match self {
            Self::Read(p, _) => size(p),
            Self::Change(p, _) => size(p),
            Self::Inspect(p, _) => size(p),
            Self::SessionChange(p, _) => size(p),
            Self::Configure(p) => size(p),
            Self::SavedProfile(p) => size(p),
            Self::Mcp(p, _) => size(p),
            Self::Send(d) | Self::Expand(d) => size(&d.request) + d.raw.len(),
            Self::Approve(p) => size(p),
            Self::Question(p) => size(p),
            Self::Agent(p, _) => size(p),
            Self::Discover(p) => size(p),
            Self::UseSkill(p) => size(p),
            Self::Rewind(p) => size(p),
            Self::Resume(id) => id.len(),
            // Image decoding has its own source/dimension/output bounds and only
            // one staged load. Reserve the complete possible wire payload.
            Self::Images { .. } => wire::MAX_FRAME,
            _ => 256,
        }
    }
}

pub(super) fn request(model: &mut Model, effect: Effect) -> bool {
    let control = effect.control();
    let recovery = matches!(effect, Effect::Snapshot);
    let bytes = effect.size();
    let Some(peer) = &mut model.remote else {
        return false;
    };
    if peer.ended
        && !matches!(
            effect,
            Effect::Resume(_) | Effect::Clear | Effect::Reconnect
        )
    {
        model.command_refused = true;
        model.push_notice(
            "The backend connection ended. Your draft is preserved; reconnect before sending.",
        );
        return false;
    }
    if peer.recovering && !control {
        model.command_refused = true;
        model.push_notice(
            "The chat is recovering its state. Your input is preserved; retry after recovery.",
        );
        return false;
    }
    let at_limit = if control {
        peer.control >= CONTROL_REQUESTS + usize::from(recovery)
    } else {
        peer.normal >= NORMAL_REQUESTS
    };
    // One fixed-size recovery request has a place past ordinary/control
    // saturation; losing stream coverage must never leave a stale UI live.
    if at_limit || (!recovery && bytes > REQUEST_BYTES.saturating_sub(peer.bytes)) {
        model.command_refused = true;
        model.push_notice(
            "A backend request is still finishing. Try this command again; input is preserved.",
        );
        return false;
    }
    if control {
        peer.control += 1;
    } else {
        peer.normal += 1;
    }
    peer.bytes += bytes;
    if effect.agent_request() {
        peer.agent_requests += 1;
    }
    match &effect {
        Effect::Agent(protocol::AgentCommand::Followup { .. }, _)
        | Effect::Send(Draft {
            target: Some(_),
            followup: true,
            ..
        }) => {
            model.pending_agent_launches += 1;
        }
        Effect::Agent(protocol::AgentCommand::Steer { .. }, _)
        | Effect::Send(Draft {
            target: Some(_),
            followup: false,
            ..
        }) => {
            model.pending_agent_steers += 1;
        }
        _ => {}
    }
    let form = matches!(
        &effect,
        Effect::Discover(_)
            | Effect::Mcp(protocol::McpCommand::Credential { .. }, _)
            | Effect::Change(
                protocol::ChangeResource::SaveModel(_)
                    | protocol::ChangeResource::UpdateModelKey { .. }
                    | protocol::ChangeResource::Search { .. },
                _
            )
    )
    .then_some(model.form_generation)
    .or_else(|| {
        matches!(&effect, Effect::SavedProfile(_))
            .then_some(())
            .and(model.model_setup.as_ref())
            .filter(|setup| {
                matches!(
                    setup.step,
                    ModelSetupStep::Saving | ModelSetupStep::Activating
                )
            })
            .map(|_| model.form_generation)
    });
    peer.pending.push_back(Pending {
        effect,
        form,
        command: model.command_draft.clone(),
    });
    true
}

enum Answer {
    Resource(Box<Resource>),
    Session(protocol::SessionResult),
    Settings(protocol::Settings),
    Mcp(protocol::McpResult),
    Sent(protocol::MessageAccepted),
    Expanded(protocol::ExpandedCommand),
    Accepted(protocol::Accepted),
    Cancelled(protocol::Cancelled),
    Discovered(Vec<protocol::DiscoveredModel>),
    Rewound(protocol::Rewound, Result<Vec<staged_images::Image>, String>),
    Points(protocol::RewindPoints),
    Snapshot(Box<protocol::PresentationSnapshot>),
    Opened(Box<Opened>),
    Images(Vec<staged_images::Image>),
    Reloaded(protocol::ExtensionsReloaded),
}
struct Opened {
    connection: Arc<Connection>,
    view: View,
    snapshot: protocol::PresentationSnapshot,
    bootstrap: protocol::Bootstrap,
}
struct Completion {
    generation: u64,
    effect: Effect,
    form: Option<u64>,
    command: Option<String>,
    result: Result<Answer, String>,
}

fn find_executable() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| error.to_string())
}

fn create(
    folder: PathBuf,
    startup: protocol::StartupOptions,
    resume: Option<String>,
    settings: Option<protocol::RestoreSettings>,
) -> protocol::CreateSession {
    protocol::CreateSession {
        folder,
        ends_with_client: true,
        resume,
        model: None,
        mode: None,
        reasoning: None,
        settings,
        startup: Some(startup),
    }
}

async fn opened(
    connection: Arc<Connection>,
    folder: PathBuf,
    mut startup: protocol::StartupOptions,
    mut resume: Option<String>,
    settings: Option<protocol::RestoreSettings>,
) -> Result<Opened, String> {
    if resume.is_none()
        && matches!(startup.resume, protocol::Resume::Latest)
        && let Resource::Sessions(sessions) = connection
            .call_async(protocol::Scope::Folder(&folder), &Read::Sessions)
            .await?
    {
        resume = sessions.first().map(|session| session.id.clone());
    }
    if resume.is_none()
        && let protocol::Resume::Id(id) = &startup.resume
    {
        resume = Some(id.clone());
    }
    startup.resume = resume.clone().map(protocol::Resume::Id).unwrap_or_default();
    let active = if let Some(id) = &resume {
        let listed = connection
            .call_async(protocol::Scope::Service, &protocol::ListSessions {})
            .await?;
        listed.sessions.into_iter().find(|live| {
            (live.conversation.as_ref().unwrap_or(&live.session) == id)
                && live
                    .about
                    .as_ref()
                    .is_some_and(|about| about.folder == folder)
        })
    } else {
        None
    };
    let (mut view, _) = if let Some(live) = active {
        View::attach(
            connection.clone(),
            live.session,
            None,
            ViewLimits::default(),
        )
        .await?
    } else {
        match View::create(
            connection.clone(),
            &create(folder.clone(), startup, resume.clone(), settings),
            ViewLimits::default(),
        )
        .await
        {
            Ok((view, _, attached)) => (view, attached),
            Err(error) => {
                // Only the exact concurrent live incarnation in this folder can
                // satisfy a failed resume. Never retire somebody else's chat.
                let Some(id) = resume else {
                    return Err(error);
                };
                let listed = connection
                    .call_async(protocol::Scope::Service, &protocol::ListSessions {})
                    .await?;
                let Some(live) = listed.sessions.into_iter().find(|live| {
                    live.conversation.as_ref().unwrap_or(&live.session) == &id
                        && live
                            .about
                            .as_ref()
                            .is_some_and(|about| about.folder == folder)
                }) else {
                    return Err(error);
                };
                View::attach(
                    connection.clone(),
                    live.session,
                    None,
                    ViewLimits::default(),
                )
                .await?
            }
        }
    };
    let snapshot = view.call(&protocol::GetPresentation {}).await?;
    let cursor = snapshot
        .cursor
        .clone()
        .ok_or("The backend returned no presentation barrier")?;
    view.covered_through(cursor)?;
    let protocol::SessionResult::Bootstrap(bootstrap) = view.call(&Inspect::Bootstrap).await?
    else {
        return Err("The backend returned an invalid terminal bootstrap".into());
    };
    Ok(Opened {
        connection,
        view,
        snapshot,
        bootstrap,
    })
}

struct Context {
    connection: Arc<Connection>,
    chat: Chat,
    folder: PathBuf,
    startup: protocol::StartupOptions,
    settings: Option<protocol::RestoreSettings>,
    backend: Arc<Backend>,
    conversation: String,
}

async fn execute(context: Context, effect: &Effect) -> Result<Answer, String> {
    let Context {
        connection,
        chat,
        folder,
        startup,
        settings,
        backend,
        conversation,
    } = context;
    let connection = if matches!(
        effect,
        Effect::Resume(_) | Effect::Reconnect | Effect::Clear
    ) {
        tokio::task::spawn_blocking(move || backend.connection())
            .await
            .map_err(|error| error.to_string())??
    } else {
        connection
    };
    match effect {
        Effect::Read(p, _) => connection
            .call_chat(&chat, &protocol::ChatResource(p.clone()))
            .await
            .map(|resource| Answer::Resource(Box::new(resource))),
        Effect::Change(p, _) => connection
            .call_async(protocol::Scope::Folder(&folder), p)
            .await
            .map(|resource| Answer::Resource(Box::new(resource))),
        Effect::Inspect(p, _) => connection.call_chat(&chat, p).await.map(Answer::Session),
        Effect::SessionChange(p, _) => connection.call_chat(&chat, p).await.map(Answer::Session),
        Effect::Configure(p) => connection.call_chat(&chat, p).await.map(Answer::Settings),
        Effect::Reload(_) => connection
            .call_chat(&chat, &protocol::ReloadExtensions {})
            .await
            .map(Answer::Reloaded),
        Effect::SavedProfile(p) => connection.call_chat(&chat, p).await.map(Answer::Settings),
        Effect::Mcp(p, _) => connection.call_chat(&chat, p).await.map(Answer::Mcp),
        Effect::Send(d) => {
            if let Some(agent) = &d.target {
                let command = if d.followup {
                    protocol::AgentCommand::Followup {
                        agent: agent.clone(),
                        text: d.request.content.clone(),
                    }
                } else {
                    protocol::AgentCommand::Steer {
                        agent: agent.clone(),
                        text: d.request.content.clone(),
                    }
                };
                connection
                    .call_chat(&chat, &command)
                    .await
                    .map(Answer::Accepted)
            } else {
                connection
                    .call_chat(&chat, &d.request)
                    .await
                    .map(Answer::Sent)
            }
        }
        Effect::Expand(d) => connection
            .call_chat(
                &chat,
                &protocol::ExpandCommand {
                    typed: d.request.content.clone(),
                },
            )
            .await
            .map(Answer::Expanded),
        Effect::Approve(p) => connection.call_chat(&chat, p).await.map(Answer::Accepted),
        Effect::Question(p) => connection.call_chat(&chat, p).await.map(Answer::Accepted),
        Effect::Cancel => connection
            .call_chat(&chat, &protocol::Cancel {})
            .await
            .map(Answer::Cancelled),
        Effect::Abort => connection
            .call_chat(&chat, &protocol::AbortTurn {})
            .await
            .map(Answer::Accepted),
        Effect::Agent(p, _) => connection.call_chat(&chat, p).await.map(Answer::Accepted),
        Effect::Discover(p) => connection
            .call_async(protocol::Scope::Folder(&folder), p)
            .await
            .map(Answer::Discovered),
        Effect::UseSkill(p) => connection
            .call_chat(&chat, p)
            .await
            .map(|resource| Answer::Resource(Box::new(resource))),
        Effect::Rewind(p) => {
            let rewound = connection.call_chat(&chat, p).await?;
            let images = rewound.images.clone();
            let staged = tokio::task::spawn_blocking(move || staged_images::restage(images))
                .await
                .map_err(|error| error.to_string())?;
            Ok(Answer::Rewound(rewound, staged))
        }
        Effect::Points => connection
            .call_chat(&chat, &protocol::GetRewindPoints {})
            .await
            .map(Answer::Points),
        Effect::Snapshot => connection
            .call_chat(&chat, &protocol::GetPresentation {})
            .await
            .map(|snapshot| Answer::Snapshot(Box::new(snapshot))),
        Effect::Resume(id) => opened(connection, folder, startup, Some(id.clone()), None)
            .await
            .map(|o| Answer::Opened(Box::new(o))),
        Effect::Reconnect => opened(connection, folder, startup, Some(conversation), settings)
            .await
            .map(|o| Answer::Opened(Box::new(o))),
        Effect::Clear => {
            let mut startup = startup;
            startup.resume = protocol::Resume::None;
            opened(connection, folder, startup, None, settings)
                .await
                .map(|o| Answer::Opened(Box::new(o)))
        }
        Effect::Images { source, .. } => staged_images::load(source).await.map(Answer::Images),
    }
}

fn launch(model: &mut Model, jobs: &mut JoinSet<Completion>) {
    let Some(peer) = &mut model.remote else {
        return;
    };
    while let Some(Pending {
        effect,
        form,
        command,
    }) = peer.pending.pop_front()
    {
        let (connection, chat, folder, startup, generation) = (
            peer.connection.clone(),
            peer.chat.clone(),
            peer.folder.clone(),
            peer.startup.clone(),
            peer.generation,
        );
        let (backend, conversation) = (peer.backend.clone(), peer.conversation.clone());
        let settings = peer.settings.as_ref().map(|s| protocol::RestoreSettings {
            profile: Some(s.profile.clone()),
            mode: Some(s.mode),
            reasoning: Some(s.reasoning),
            effort: Some(s.effort),
            streaming: Some(s.streaming),
        });
        jobs.spawn(async move {
            let result = execute(
                Context {
                    connection,
                    chat,
                    folder,
                    startup,
                    settings,
                    backend,
                    conversation,
                },
                &effect,
            )
            .await;
            Completion {
                generation,
                effect,
                form,
                command,
                result,
            }
        });
    }
}

pub(crate) async fn run(
    folder: PathBuf,
    startup: protocol::StartupOptions,
    setup: bool,
) -> anyhow::Result<()> {
    let folder = folder.canonicalize()?;
    let backend = Backend::for_surface(
        find_executable,
        &[
            "terminal-controls",
            "replay-stream",
            "application-resources",
            "session-inspection",
            "live-conversation",
            "shared-live-state",
        ],
    );
    eprintln!("Connecting to Medha…");
    let connect = backend.clone();
    let connection = tokio::task::spawn_blocking(move || connect.connection())
        .await?
        .map_err(anyhow::Error::msg)?;
    let opened = opened(
        connection.clone(),
        folder.clone(),
        startup.clone(),
        None,
        None,
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let Opened {
        view: mut stream,
        snapshot,
        bootstrap,
        ..
    } = opened;
    theme::set(theme::detect());
    let logs = crate::config::state_dir(&folder)?.join("logs");
    let (mut terminal, mut redirect) = tty::init(&logs.join("tui-stdout.log"))?;
    let mut model = Model::new(
        "Medha".into(),
        None,
        kernel::ReasoningConfig::default(),
        lockfile::UiConfig {
            show_thinking: bootstrap.show_thinking,
            full_transparency: bootstrap.full_transparency,
        },
        tool_viz(&bootstrap),
        WorkspaceView {
            root: folder.clone(),
            execution: bootstrap.execution_backend.clone(),
        },
    );
    model.remote = Some(Peer {
        backend,
        connection,
        chat: stream.chat(),
        folder,
        startup,
        generation: 1,
        conversation: snapshot.conversation.clone(),
        turn: snapshot.turn,
        live: protocol::LiveState {
            agents: Vec::new(),
            tasks: Vec::new(),
            pending_patches: 0,
        },
        models: protocol::ModelCatalogue {
            profiles: Vec::new(),
            search: protocol::SearchProvider::Duckduckgo,
            searxng_url: None,
        },
        plugins: protocol::PluginCatalogue {
            plugins: Vec::new(),
            notices: Vec::new(),
            commands: Vec::new(),
            hook_review: None,
        },
        skills: None,
        mcp: Vec::new(),
        patches: Vec::new(),
        settings: None,
        sending: false,
        agent_requests: 0,
        normal: 0,
        control: 0,
        bytes: 0,
        pending: VecDeque::new(),
        live_pending: false,
        recovering: false,
        ended: false,
        approvals_answering: Default::default(),
        questions_answering: Default::default(),
        questions: VecDeque::new(),
        settled_turn: 0,
        pending_steers: Vec::new(),
    });
    model.memory_enabled = bootstrap.memory_enabled;
    restore_snapshot(&mut model, snapshot);
    if setup || model.model.is_empty() {
        begin_model_setup(&mut model);
    }
    request(&mut model, Effect::Read(Read::Plugins, After::Startup));
    request(&mut model, Effect::Read(Read::Models, After::Startup));
    let mut jobs = JoinSet::new();
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(REDRAW_INTERVAL);
    let mut redraw = true;
    let result = async {
        while !model.should_quit {
            launch(&mut model, &mut jobs);
            let paused = model.remote.as_ref().is_some_and(|p| p.recovering || p.ended);
            tokio::select! {
                event = events.next() => { redraw = true; match event {
                    Some(Ok(CtEvent::Key(key))) => input::handle_key(&mut model, key),
                    Some(Ok(CtEvent::Paste(text))) => input::handle_paste(&mut model, text),
                    Some(Ok(CtEvent::Mouse(mouse))) => match mouse.kind { MouseEventKind::ScrollUp => model.scroll_by(-2), MouseEventKind::ScrollDown => model.scroll_by(2), _ => input::handle_mouse(&mut model, mouse) },
                    Some(Ok(CtEvent::Resize(_, h))) => { model.viewport_height = h as usize; model.text_selection = None; model.invalidate_all_renders(); },
                    Some(Err(error)) => return Err(anyhow::Error::from(error)), None => break, _ => {}
                } },
                completion = jobs.join_next(), if !jobs.is_empty() => if let Some(completion) = completion {
                    redraw = true;
                    match completion { Ok(done) => completed(&mut model, &mut stream, done), Err(error) => return Err(anyhow::anyhow!("terminal request task failed: {error}")) }
                },
                said = stream.recv(), if !paused => { redraw = true; match said { Some(said) => observed(&mut model, said)?, None => ended(&mut model, None) } },
                _ = ticker.tick() => { redraw |= tick(&mut model); },
            }
            if let Some(text) = model.pending_clipboard.take() {
                let status = match tty::copy_to_clipboard(&mut terminal, &text) { Ok(()) => format!("copied {} chars", text.chars().count()), Err(_) => "clipboard unavailable".into() };
                model.clipboard_status = Some((status, Instant::now() + Duration::from_secs(2)));
            }
            let animated = model.running || model.intro_frame.is_some() || model.agent_runs.iter().any(|agent| agent.is_running()) || model.images.is_loading()
                || model.clipboard_status.as_ref().is_some_and(|(_, until)| Instant::now() < *until);
            if (redraw || model.dirty || animated) && model.last_redraw.elapsed() >= REDRAW_INTERVAL || (model.pending_approval().is_some() && !model.approval_ready) {
                terminal.draw(|frame| view(frame, &mut model))?;
                model.last_redraw = Instant::now();
                redraw = false;
            }
        }
        Ok(())
    }.await;
    // Restore the user's terminal before waiting on any backend operation.
    tty::restore(&mut terminal, &mut redirect);
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    drop(stream); // Detach; the last viewer lifetime rule belongs to the service.
    if let Some(returned) = model.recovery_draft.take() {
        // Retain the text until publication completes. A full disk or an
        // unavailable temporary directory must not turn recovery into silent
        // draft loss. The bounded, escaped fallback is written only to the
        // user's restored terminal, never to the backend log.
        let drafts = Arc::new((model.input, model.pastes, returned));
        let saving = Arc::clone(&drafts);
        let recovery =
            tokio::task::spawn_blocking(move || save_recovery(&saving.0, &saving.1, &saving.2))
                .await;
        return match recovery {
            Ok(Ok(path)) => Err(anyhow::anyhow!(
                "The terminal detached because returned text exceeded its composer limit. Your current and returned drafts are saved in {}",
                path.display()
            )),
            error => {
                let error = match error {
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                let output = write_recovery_fallback(
                    &mut std::io::stderr().lock(),
                    &drafts.0,
                    &drafts.1,
                    &drafts.2,
                );
                Err(anyhow::anyhow!(match output {
                    Ok(()) => format!(
                        "Draft recovery could not be saved ({error}). Both drafts are printed above as escaped JSON; copy them before closing this terminal."
                    ),
                    Err(output) => format!(
                        "Draft recovery failed ({error}), and writing the drafts to this terminal failed ({output})."
                    ),
                }))
            }
        };
    }
    result
}

fn write_recovery_fallback(
    output: &mut impl std::io::Write,
    input: &str,
    pastes: &[String],
    returned: &str,
) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct Drafts<'a> {
        current_composer: &'a str,
        pastes: &'a [String],
        returned_unsent_text: &'a str,
    }
    serde_json::to_writer(
        &mut *output,
        &Drafts {
            current_composer: input,
            pastes,
            returned_unsent_text: returned,
        },
    )?;
    writeln!(output)?;
    output.flush()
}

fn save_recovery(input: &str, pastes: &[String], returned: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    // tempfile creates a private, unpredictable file (0600 on Unix). Retain
    // one recovery artifact; do not accumulate an unbounded in-memory backlog.
    let mut file = tempfile::Builder::new()
        .prefix("medha-draft-recovery-")
        .suffix(".txt")
        .tempfile()?;
    writeln!(file, "Current composer:\n{input}")?;
    for (index, paste) in pastes.iter().enumerate() {
        writeln!(file, "\nPaste #{index}:\n{paste}")?;
    }
    writeln!(file, "\nReturned unsent text:\n{returned}")?;
    file.as_file().sync_all()?;
    let (_, path) = file.keep().map_err(|error| error.error)?;
    Ok(path)
}

fn tool_viz(bootstrap: &protocol::Bootstrap) -> HashMap<String, ToolViz> {
    bootstrap
        .tools
        .iter()
        .map(|tool| {
            (
                tool.name.clone(),
                ToolViz {
                    icon: tool.icon.clone(),
                    category: match tool.category.as_str() {
                        "read" => ToolCategory::Read,
                        "write" => ToolCategory::Write,
                        "shell" => ToolCategory::Shell,
                        "web" => ToolCategory::Web,
                        "search" => ToolCategory::Search,
                        "vcs" => ToolCategory::Vcs,
                        "diagnostic" => ToolCategory::Diagnostic,
                        "plan" => ToolCategory::Plan,
                        "other" => ToolCategory::Other,
                        _ => ToolCategory::Other,
                    },
                },
            )
        })
        .collect()
}

pub(super) fn item(item: protocol::PresentationItem) -> Item {
    use protocol::PresentationItem as P;
    match item {
        P::User { text } => Item::User(text),
        P::Assistant { text } => Item::Assistant(text),
        P::Reasoning { text } => Item::Thinking(text),
        P::Notice { text } => Item::Notice(text),
        P::ToolCall { id, tool, args } => Item::ToolCall { id, tool, args },
        P::ToolResult {
            id,
            tool,
            ok,
            payload,
        } => Item::ToolResult {
            id,
            tool,
            ok,
            payload,
        },
        P::Compaction {
            before,
            after,
            summarized,
            summary,
        } => Item::Compaction {
            before,
            after,
            summarized,
            summary,
        },
        P::Verify { ok, summary } => Item::Verify { ok, summary },
        P::PreviewOmitted { subject, id, bytes } => Item::Notice(format!(
            "{subject} preview omitted ({bytes} bytes{}); the full content remains in saved history.",
            id.map(|id| format!(", {id}")).unwrap_or_default()
        )),
    }
}

fn settings(model: &mut Model, settings: protocol::Settings) {
    model.model = settings.model.clone();
    model.active_profile = settings.profile.clone();
    model.max_ctx = settings.context_limit;
    if let Some(protocol) = &settings.protocol
        && let Ok(protocol) = protocol.parse()
    {
        model.protocol = protocol;
    }
    model.autonomy = match settings.mode {
        protocol::Mode::Plan => kernel::AutonomyLevel::Plan,
        protocol::Mode::Careful => kernel::AutonomyLevel::Careful,
        protocol::Mode::Normal => kernel::AutonomyLevel::Normal,
        protocol::Mode::Yolo => kernel::AutonomyLevel::Yolo,
    };
    model.reasoning = kernel::ReasoningConfig {
        enabled: match settings.reasoning {
            protocol::Reasoning::Auto => None,
            protocol::Reasoning::On => Some(true),
            protocol::Reasoning::Off => Some(false),
        },
        effort: effort(settings.effort),
    };
    model.reasoning_support = match settings.reasoning_support {
        protocol::ReasoningSupport::Unverified => kernel::ReasoningSupport::Unknown,
        protocol::ReasoningSupport::Unsupported => kernel::ReasoningSupport::Unsupported,
        protocol::ReasoningSupport::Effort => kernel::ReasoningSupport::Effort,
    };
    model.streaming = settings.streaming;
    if let Some(peer) = &mut model.remote {
        peer.settings = Some(settings);
    }
    model.dirty = true;
}

pub(super) fn effort(value: protocol::Effort) -> Option<kernel::ReasoningEffort> {
    use protocol::Effort as E;
    match value {
        E::Auto => None,
        E::None => Some(kernel::ReasoningEffort::None),
        E::Minimal => Some(kernel::ReasoningEffort::Minimal),
        E::Low => Some(kernel::ReasoningEffort::Low),
        E::Medium => Some(kernel::ReasoningEffort::Medium),
        E::High => Some(kernel::ReasoningEffort::High),
        E::XHigh => Some(kernel::ReasoningEffort::XHigh),
        E::Max => Some(kernel::ReasoningEffort::Max),
        E::Ultra => Some(kernel::ReasoningEffort::Ultra),
    }
}

fn approval(model: &mut Model, prompt: protocol::ApprovalPrompt) {
    let previous = model.pending_approvals.front().map(|pending| {
        let ApprovalResponder::Remote(prompt) = &pending.responder;
        prompt.gate_id
    });
    model.pending_approvals.retain(|pending| !matches!(&pending.responder, ApprovalResponder::Remote(old) if old.gate_id == prompt.gate_id));
    model.pending_approvals.push_back(PendingApproval {
        action: prompt.action.clone(),
        detail: prompt.detail.clone(),
        escalated: prompt.escalated,
        responder: ApprovalResponder::Remote(prompt),
    });
    if previous.is_none() {
        model.approval_sel = 0;
        model.approval_ready = false;
        model.approval_expanded = false;
    }
    model.dirty = true;
}

fn question(model: &mut Model, prompt: protocol::QuestionPrompt) -> Result<(), String> {
    let peer = model.remote.as_mut().ok_or("No backend viewer")?;
    peer.questions
        .retain(|old| old.question_id != prompt.question_id);
    peer.questions.push_back(prompt);
    next_question(model)
}

fn next_question(model: &mut Model) -> Result<(), String> {
    if model.clarify.is_some() {
        return Ok(());
    }
    let Some(prompt) = model
        .remote
        .as_ref()
        .and_then(|peer| peer.questions.front())
        .cloned()
    else {
        return Ok(());
    };
    if prompt.questions.is_empty() {
        return Err("The backend returned an empty question form".into());
    }
    let questions: Vec<kernel::Question> = prompt
        .questions
        .into_iter()
        .map(|question| kernel::Question {
            prompt: question.prompt,
            header: question.header,
            multi_select: question.multi_select,
            options: question
                .options
                .into_iter()
                .map(|option| kernel::QOption {
                    label: option.label,
                    description: option.description,
                    recommended: option.recommended,
                })
                .collect(),
        })
        .collect();
    let drafts = questions
        .iter()
        .map(|question| ClarifyDraft {
            selected: if question.multi_select {
                Vec::new()
            } else {
                question
                    .options
                    .iter()
                    .position(|option| option.recommended)
                    .into_iter()
                    .collect()
            },
            other: None,
        })
        .collect();
    model.clarify = Some(ClarifyState {
        drafts,
        questions,
        idx: 0,
        cursor: 0,
        entering_other: false,
        other_input: String::new(),
        other_cursor: 0,
        validation: None,
        responder: QuestionResponder::Remote(prompt.question_id),
    });
    model.dirty = true;
    Ok(())
}

fn resolve_approval(model: &mut Model, id: u64) {
    let first = model.pending_approvals.front().is_some_and(|pending| matches!(&pending.responder, ApprovalResponder::Remote(prompt) if prompt.gate_id == id));
    model.pending_approvals.retain(|pending| !matches!(&pending.responder, ApprovalResponder::Remote(prompt) if prompt.gate_id == id));
    if let Some(peer) = &mut model.remote {
        peer.approvals_answering.remove(&id);
    }
    if first {
        model.approval_ready = false;
        model.approval_sel = 0;
        model.approval_expanded = false;
    }
    model.dirty = true;
}

fn resolve_question(model: &mut Model, id: u64) -> Result<(), String> {
    if let Some(peer) = &mut model.remote {
        peer.questions.retain(|prompt| prompt.question_id != id);
        peer.questions_answering.remove(&id);
    }
    if model.clarify.as_ref().is_some_and(
        |state| matches!(&state.responder, QuestionResponder::Remote(current) if *current == id),
    ) {
        model.clarify = None;
    }
    model.dirty = true;
    next_question(model)
}

fn restore_snapshot(model: &mut Model, snapshot: protocol::PresentationSnapshot) {
    let focus = model.focus.clone();
    let same_conversation = model
        .remote
        .as_ref()
        .is_some_and(|peer| peer.conversation == snapshot.conversation);
    let pending_launches = model.pending_agent_launches;
    model.clear_session_panes();
    if same_conversation {
        model.pending_agent_launches = pending_launches;
    }
    model.pending_agent_steers = snapshot.agents.iter().map(|pane| pane.pending_steers).sum();
    model.items.clear();
    model.parked_main.clear();
    for row in snapshot.items {
        model.push_main_item(item(row));
    }
    if snapshot.omitted_items > 0 {
        model.push_main_notice(format!(
            "{} older presentation rows remain in saved history.",
            snapshot.omitted_items
        ));
    }
    for pane in snapshot.agents {
        if let Ok(path) = orchestrator::AgentPath::parse(&pane.path) {
            let mut rows: VecDeque<_> = pane
                .items
                .into_iter()
                .map(|row| Entry::new(item(row)))
                .collect();
            if pane.omitted_items > 0 {
                rows.push_front(Entry::new(Item::Notice(format!(
                    "{} older rows remain in this agent's history.",
                    pane.omitted_items
                ))));
            }
            model.agent_panes.insert(path, rows);
        }
    }
    model.trim_agent_views();
    backend_features::refresh_roster(model, snapshot.roster);
    if focus
        .as_ref()
        .is_some_and(|path| model.agent_panes.contains_key(path))
    {
        model.focus_pane(focus);
    }
    model.running = snapshot.running;
    model.force_aborting = snapshot.force_aborting;
    model.cancelling = false;
    model.turn_started = snapshot.running.then(Instant::now);
    let metrics = snapshot.metrics;
    model.compacting = metrics.compacting;
    model.current_tool = metrics.current_tool;
    model.cache = metrics
        .cached_prompt_tokens
        .zip(metrics.cache_prompt_tokens);
    model.cache_unreported_attempts = metrics.cache_unreported_attempts;
    model.cache_last_usage = metrics.last_usage.map(|usage| kernel::Usage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens.unwrap_or(0),
        total_tokens: usage.total_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
    });
    model.cost_usd = metrics.cost_usd.map(|usd| (usd, metrics.cost_indicative));
    model.context_pressure = metrics
        .context_pressure
        .map(|pressure| kernel::ContextPressure {
            input_tokens: pressure.input_tokens,
            input_limit: pressure.input_limit,
            usable_input_tokens: pressure.usable_input_tokens,
            quality: count_quality(pressure.quality),
        });
    model.reasoning_received_this_turn = metrics.reasoning_received_this_turn;
    model.last_turn_reasoning_received = metrics.last_turn_reasoning_received;
    if let Some(settings_) = snapshot.settings {
        settings(model, settings_);
    }
    let old_front = model.pending_approvals.front().map(|pending| {
        let ApprovalResponder::Remote(prompt) = &pending.responder;
        prompt.gate_id
    });
    let new_front = snapshot.approvals.first().map(|prompt| prompt.gate_id);
    let (ready, selected, expanded) = (
        model.approval_ready,
        model.approval_sel,
        model.approval_expanded,
    );
    model.pending_approvals.clear();
    for prompt in snapshot.approvals {
        approval(model, prompt);
    }
    if old_front != new_front {
        model.approval_ready = false;
        model.approval_sel = 0;
    } else {
        model.approval_ready = ready;
        model.approval_sel = selected;
        model.approval_expanded = expanded;
    }
    let peer = model.remote.as_mut().expect("backend viewer");
    peer.conversation = snapshot.conversation;
    peer.turn = snapshot.turn;
    peer.settled_turn = if snapshot.running {
        snapshot.turn.saturating_sub(1)
    } else {
        snapshot.turn
    };
    peer.pending_steers = snapshot.pending_steers;
    peer.recovering = false;
    peer.questions = snapshot.questions.into();
    let still_open = model
        .clarify
        .as_ref()
        .is_some_and(|state| match &state.responder {
            QuestionResponder::Remote(id) => peer
                .questions
                .iter()
                .any(|question| question.question_id == *id),
        });
    if !still_open {
        model.clarify = None;
    }
    if let Err(error) = next_question(model) {
        ended(model, Some(error));
    }
    model.welcome = model.items.is_empty() && !model.running;
    model.invalidate_all_renders();
}

fn count_quality(quality: protocol::CountQuality) -> kernel::TokenCountQuality {
    match quality {
        protocol::CountQuality::Authoritative => kernel::TokenCountQuality::Authoritative,
        protocol::CountQuality::ProviderEstimate => kernel::TokenCountQuality::ProviderEstimate,
        protocol::CountQuality::LocalEstimate => kernel::TokenCountQuality::LocalEstimate,
    }
}

fn recover(model: &mut Model) {
    if model.remote.as_ref().is_some_and(|peer| peer.recovering) {
        return;
    }
    if request(model, Effect::Snapshot)
        && let Some(peer) = &mut model.remote
    {
        peer.recovering = true;
    }
}
fn ended(model: &mut Model, reason: Option<String>) {
    if let Some(peer) = &mut model.remote {
        peer.ended = true;
    }
    model.running = false;
    model.current_tool = None;
    model.push_main_notice(reason.unwrap_or_else(|| {
        "The backend chat ended. Your input is preserved. Use /reconnect to rejoin it.".into()
    }));
}

fn observed(model: &mut Model, said: Said) -> anyhow::Result<()> {
    let frame = match said {
        Said::Frame(frame) | Said::Event { frame, .. } => frame,
        Said::Ended(reason) => {
            ended(model, reason);
            return Ok(());
        }
    };
    let method = frame["method"].as_str().unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    let malformed = || anyhow::anyhow!("The backend returned an invalid {method} event");
    match method {
        "event" => turn_event(
            model,
            serde_json::from_value(params).map_err(|_| malformed())?,
        ),
        "settings" => settings(
            model,
            serde_json::from_value(params).map_err(|_| malformed())?,
        ),
        "approval" => approval(
            model,
            serde_json::from_value(params).map_err(|_| malformed())?,
        ),
        "approval.resolved" => {
            let resolved: protocol::ApprovalResolved =
                serde_json::from_value(params).map_err(|_| malformed())?;
            resolve_approval(model, resolved.gate_id);
        }
        "question" => question(
            model,
            serde_json::from_value(params).map_err(|_| malformed())?,
        )
        .map_err(anyhow::Error::msg)?,
        "question.answered" => {
            let resolved: protocol::QuestionAnswered =
                serde_json::from_value(params).map_err(|_| malformed())?;
            resolve_question(model, resolved.question_id).map_err(anyhow::Error::msg)?;
        }
        "agent.step" => {
            let event: protocol::AgentEvent =
                serde_json::from_value(params).map_err(|_| malformed())?;
            if event.surface_session.as_ref().is_none_or(|owner| {
                model
                    .remote
                    .as_ref()
                    .is_some_and(|peer| peer.conversation == *owner)
            }) {
                let path = orchestrator::AgentPath::parse(&event.path).map_err(|_| malformed())?;
                model.push_agent_step(path, event.step);
            }
        }
        "agents" => {
            let roster =
                serde_json::from_value(params["agents"].clone()).map_err(|_| malformed())?;
            backend_features::refresh_roster(model, roster);
        }
        "session.rewound" => recover(model),
        "mcp.auth" => {
            if let (Some(id), Some(url)) = (params["id"].as_str(), params["url"].as_str()) {
                model.push_notice(format!(
                    "Signing in to '{id}'. If the browser did not open, use:\n{url}"
                ));
            }
        }
        // Transport replies are separately routed and optional future events
        // cannot become command dispatch or safety decisions.
        _ => {}
    }
    model.dirty = true;
    Ok(())
}

fn turn_event(model: &mut Model, event: protocol::TurnEvent) {
    use protocol::TurnEvent as E;
    match event {
        E::Started { turn } => {
            if let Some(peer) = &mut model.remote {
                peer.turn = turn;
            }
            model.running = true;
            model.cancelling = false;
            model.turn_started = Some(Instant::now());
            model.reasoning_received_this_turn = false;
            // A new answer must not extend the preceding turn's answer.
            model.streamed_this_turn = 0;
        }
        E::PresentationReset { .. } | E::Restarted => recover(model),
        E::User { content } => model.push_main_item(Item::User(content)),
        E::Text { delta } => {
            model.current_tool = None;
            model.push_text_delta(&delta);
        }
        E::Reasoning { delta } => model.push_thinking_delta(&delta),
        E::Notice { text } => model.push_main_notice(text),
        E::ToolStarted { tool, target } => model.current_tool = Some((tool, target)),
        E::ToolCall { id, tool, args } => model.push_main_item(Item::ToolCall { id, tool, args }),
        E::ToolResult {
            id,
            tool,
            ok,
            payload,
        } => {
            model.current_tool = None;
            model.push_main_item(Item::ToolResult {
                id,
                tool,
                ok,
                payload,
            });
        }
        E::Usage {
            prompt_tokens,
            total_tokens,
            completion_tokens,
            cached_prompt_tokens,
        } => {
            model.cache_last_usage = Some(kernel::Usage {
                prompt_tokens,
                total_tokens,
                completion_tokens: completion_tokens.unwrap_or(0),
                cached_prompt_tokens,
            });
            if let Some(cached) = cached_prompt_tokens {
                let (hits, prompt) = model.cache.unwrap_or_default();
                model.cache = Some((
                    hits.saturating_add(u64::from(cached.min(prompt_tokens))),
                    prompt.saturating_add(u64::from(prompt_tokens)),
                ));
            } else {
                model.cache_unreported_attempts = model.cache_unreported_attempts.saturating_add(1);
            }
        }
        E::Cost {
            total_usd,
            indicative,
        } => model.cost_usd = Some((total_usd, indicative)),
        E::ContextPressure {
            input_tokens,
            input_limit,
            usable_input_tokens,
            quality,
            ..
        } => {
            model.context_pressure = Some(kernel::ContextPressure {
                input_tokens,
                input_limit,
                usable_input_tokens,
                quality: count_quality(quality),
            })
        }
        E::Verify { ok, summary } => model.push_main_item(Item::Verify { ok, summary }),
        E::Compacting { active } => model.compacting = active,
        E::Compaction {
            before,
            after,
            summarized,
            summary,
        } => {
            model.compacting = false;
            model.push_main_item(Item::Compaction {
                before,
                after,
                summarized,
                summary,
            });
        }
        E::Queued {
            content: Some(text),
        } => {
            if let Some(peer) = &mut model.remote {
                peer.pending_steers.push(text.clone());
            }
            model.push_main_notice(format!("↳ queued for this task: {text}"));
        }
        E::Steered { content } => {
            if let Some(peer) = &mut model.remote
                && let Some(index) = peer.pending_steers.iter().position(|text| *text == content)
            {
                peer.pending_steers.remove(index);
            }
            model.remove_last_notice("↳ queued for this task:");
            model.push_main_item(Item::User(content));
        }
        E::Returned { contents } => {
            for text in &contents {
                if let Some(peer) = &mut model.remote
                    && let Some(index) = peer.pending_steers.iter().position(|old| old == text)
                {
                    peer.pending_steers.remove(index);
                }
                model.remove_last_notice("↳ queued for this task:");
            }
            restore_draft(model, contents.join("\n"));
            model.push_main_notice("Queued text returned to the input — it was not sent.");
        }
        E::Done { stopped } => {
            settled(model);
            if let Some(stopped) = stopped {
                model.push_main_notice(format!("Turn stopped: {stopped}"));
            }
        }
        E::Cancelled => {
            settled(model);
            model.push_main_notice("⏹ interrupted — current tools settled");
        }
        E::Error { message } => {
            settled(model);
            model.push_main_notice(format!("Turn failed: {message}"));
        }
        E::AbortSettled => {
            model.force_aborting = false;
            settled(model);
        }
        E::AbortSlow => {
            model.force_aborting = true;
            model.push_main_notice("Force stop is still quiescing owned work…");
        }
        E::Continued { stopped } => model.push_main_notice(format!("Continuing after {stopped}")),
        _ => {}
    }
}
fn settled(model: &mut Model) {
    if let Some(peer) = &mut model.remote {
        peer.settled_turn = peer.settled_turn.max(peer.turn);
    }
    model.running = false;
    model.cancelling = false;
    model.current_tool = None;
    model.compacting = false;
    model.turn_started = None;
    model.last_turn_reasoning_received = Some(model.reasoning_received_this_turn);
}
pub(super) fn restore_draft(model: &mut Model, text: String) {
    if text.is_empty()
        || model
            .resolve_pastes(&model.input)
            .is_ok_and(|input| input.trim() == text.trim())
    {
        return;
    }
    let used = model.input.len() + model.pastes.iter().map(String::len).sum::<usize>();
    if text.len().saturating_add(64) > MAX_COMPOSER_BYTES.saturating_sub(used) {
        if model.recovery_draft.is_none() {
            model.recovery_draft = Some(text);
        }
        model.should_quit = true;
        return;
    }
    if !model.input.is_empty() {
        model.input.push('\n');
    }
    let count = text.chars().count();
    if count > PASTE_COLLAPSE_THRESHOLD {
        let index = model.pastes.len();
        model
            .input
            .push_str(&format!("[paste #{index}: {count} chars]"));
        model.pastes.push(text);
    } else {
        model.input.push_str(&text);
    }
    model.cursor = model.input.len();
}

fn completed(model: &mut Model, stream: &mut View, done: Completion) {
    let Some(peer) = &mut model.remote else {
        return;
    };
    if done.effect.control() {
        peer.control = peer.control.saturating_sub(1);
    } else {
        peer.normal = peer.normal.saturating_sub(1);
    }
    peer.bytes = peer.bytes.saturating_sub(done.effect.size());
    if done.effect.agent_request() {
        peer.agent_requests = peer.agent_requests.saturating_sub(1);
    }
    if done.generation != peer.generation {
        return;
    }
    match &done.effect {
        Effect::Agent(protocol::AgentCommand::Followup { .. }, _)
        | Effect::Send(Draft {
            target: Some(_),
            followup: true,
            ..
        }) => {
            model.pending_agent_launches = model.pending_agent_launches.saturating_sub(1);
        }
        Effect::Agent(protocol::AgentCommand::Steer { .. }, _)
        | Effect::Send(Draft {
            target: Some(_),
            followup: false,
            ..
        }) if done.result.is_err()
            || matches!(&done.result, Ok(Answer::Accepted(a)) if !a.accepted) =>
        {
            model.pending_agent_steers = model.pending_agent_steers.saturating_sub(1);
        }
        _ => {}
    }
    if matches!(done.effect, Effect::Inspect(Inspect::Live, _)) {
        peer.live_pending = false;
    }
    if done
        .form
        .is_some_and(|generation| generation != model.form_generation)
    {
        model.push_notice("An earlier setup request finished after that form was closed. Reopen settings to inspect any saved change.");
        return;
    }
    match done.result {
        Err(error) => {
            match &done.effect {
                Effect::Send(draft) | Effect::Expand(draft) => {
                    peer.sending = false;
                    model.plugin_command_busy = false;
                    restore_draft(model, draft.raw.clone());
                }
                Effect::Agent(_, Some(raw)) => restore_draft(model, raw.clone()),
                Effect::Approve(answer) => {
                    peer.approvals_answering.remove(&answer.gate_id);
                }
                Effect::Question(answer) => {
                    peer.questions_answering.remove(&answer.question_id);
                }
                Effect::Resume(_) | Effect::Clear | Effect::Rewind(_) | Effect::Reconnect => {
                    model.session_op = None
                }
                Effect::Snapshot => {
                    peer.recovering = false;
                    ended(model, Some(error.clone()));
                }
                Effect::Images { generation, source } => {
                    model.images.failed(*generation);
                    if let Some(held) = model.backend_deferred.take() {
                        restore_draft(model, held.raw);
                    }
                    if let input::ImageSource::Paths(paths) = source
                        && model.input.trim().is_empty()
                    {
                        restore_draft(
                            model,
                            paths
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    }
                }
                Effect::Discover(_) => {
                    if let Some(setup) = &mut model.model_setup {
                        setup.step = ModelSetupStep::ModelId;
                    }
                }
                _ => {}
            }
            if done.form.is_some() {
                if model
                    .model_setup
                    .as_ref()
                    .is_some_and(|setup| matches!(setup.step, ModelSetupStep::Activating))
                {
                    model.model_setup = None;
                    model.push_notice("The model is saved, but activation failed. Use /model to retry activation.");
                }
                if let Some(setup) = &mut model.model_setup
                    && matches!(setup.step, ModelSetupStep::Saving)
                {
                    setup.step = if matches!(setup.mode, ModelSetupMode::UpdateKey { .. }) {
                        ModelSetupStep::ApiKey
                    } else {
                        ModelSetupStep::ContextWindow
                    };
                }
                if let Some(setup) = &mut model.search_setup {
                    setup.step = SearchSetupStep::Secret;
                }
                if let Some(form) = &mut model.mcp_credential {
                    form.saving = false;
                }
            }
            if let Some(command) = done.command {
                restore_draft(model, command);
            }
            model.push_notice(format!("Backend request: {error}"));
        }
        Ok(answer) => match answer {
            Answer::Resource(resource) => {
                let after = match done.effect {
                    Effect::Read(_, after) | Effect::Change(_, after) => after,
                    _ => After::Notice,
                };
                resource_reply(model, *resource, after);
            }
            Answer::Session(reply) => {
                let after = match done.effect {
                    Effect::Inspect(_, after) | Effect::SessionChange(_, after) => after,
                    _ => After::Notice,
                };
                session_reply(model, reply, after);
            }
            Answer::Settings(settings_) => {
                settings(model, settings_);
                if done.form.is_some() {
                    model.model_setup = None;
                    model.picker = None;
                }
                if matches!(done.effect, Effect::SavedProfile(_)) {
                    model.push_notice(format!(
                        "Active model: {} ({})",
                        model.model, model.active_profile
                    ));
                }
            }
            Answer::Mcp(reply) => {
                if done.form.is_some() {
                    model.mcp_credential = None;
                }
                let after = match done.effect {
                    Effect::Mcp(_, after) => after,
                    _ => After::Notice,
                };
                mcp_reply(model, reply, after);
            }
            Answer::Sent(accepted) => {
                peer.sending = false;
                if accepted.accepted {
                    if accepted.turn > peer.settled_turn && !accepted.steered {
                        model.running = true;
                        peer.turn = accepted.turn;
                    }
                    model.images.reset();
                    if model.input.is_empty() {
                        model.pastes.clear();
                    }
                } else if let Effect::Send(draft) = done.effect {
                    restore_draft(model, draft.raw);
                }
            }
            Answer::Expanded(expanded) => {
                model.plugin_command_busy = false;
                if let Effect::Expand(mut draft) = done.effect {
                    draft.request.content = expanded.prompt;
                    let raw = draft.raw.clone();
                    if !request(model, Effect::Send(draft)) {
                        model.remote.as_mut().expect("viewer").sending = false;
                        restore_draft(model, raw);
                    }
                }
            }
            Answer::Accepted(accepted) => match done.effect {
                Effect::Approve(answer) if accepted.accepted => {
                    resolve_approval(model, answer.gate_id)
                }
                Effect::Question(answer) if accepted.accepted => {
                    if let Err(error) = resolve_question(model, answer.question_id) {
                        ended(model, Some(error));
                    }
                }
                Effect::Approve(answer) => {
                    peer.approvals_answering.remove(&answer.gate_id);
                    model.push_notice("That approval was not accepted; its form remains open.");
                }
                Effect::Question(answer) => {
                    peer.questions_answering.remove(&answer.question_id);
                    model.push_notice("That answer was not accepted; its form remains open.");
                }
                Effect::Send(draft) => {
                    peer.sending = false;
                    if !accepted.accepted {
                        restore_draft(model, draft.raw);
                    }
                }
                Effect::Agent(_, Some(raw)) if !accepted.accepted => {
                    restore_draft(model, raw);
                    model.push_notice("The backend did not accept that agent message; it is restored to the composer.");
                }
                _ if !accepted.accepted => {
                    model.push_notice("The backend did not accept that control.")
                }
                _ => {}
            },
            Answer::Cancelled(cancelled) => {
                if !cancelled.cancelled {
                    model.cancelling = false;
                }
            }
            Answer::Discovered(models) => {
                let rows = models
                    .into_iter()
                    .map(|model| providers::openai_compat::ModelInfo {
                        id: model.id,
                        context_length: model.context_length,
                    })
                    .collect();
                if model.model_setup.is_some() {
                    model.picker = Some(Picker::new(PickerKind::ModelDiscovery(rows)));
                }
            }
            Answer::Points(points) => {
                if !model.foreground_owned() && !model.has_active_agents() {
                    let rows = points
                        .points
                        .into_iter()
                        .filter_map(|point| {
                            point.id.parse().ok().map(|at_event| RewindPoint {
                                at_event,
                                label: point.text,
                                files: point.files,
                            })
                        })
                        .collect();
                    model.picker = Some(Picker::new(PickerKind::Rewind(rows)));
                }
            }
            Answer::Rewound(rewound, staged) => {
                model.session_op = None;
                restore_draft(model, rewound.prefill);
                match staged {
                    Ok(images) => model.images.restore(images),
                    Err(error) => model.push_notice(format!("Saved attachment: {error}")),
                }
                model.push_notice(format!(
                    "Rewound; restored {} tracked file(s).",
                    rewound.restored
                ));
                recover(model);
            }
            Answer::Snapshot(snapshot) => {
                if let Some(cursor) = snapshot.cursor.clone() {
                    match stream.covered_through(cursor) {
                        Ok(()) => restore_snapshot(model, *snapshot),
                        Err(error) => ended(model, Some(error)),
                    }
                } else {
                    ended(
                        model,
                        Some("The backend returned no snapshot barrier".into()),
                    );
                }
            }
            Answer::Opened(opened) => {
                let Opened {
                    connection,
                    view,
                    snapshot,
                    bootstrap,
                } = *opened;
                peer.connection = connection;
                *stream = view;
                peer.chat = stream.chat();
                peer.generation = peer.generation.wrapping_add(1);
                peer.ended = false;
                peer.sending = false;
                peer.live_pending = false;
                peer.approvals_answering.clear();
                peer.questions_answering.clear();
                peer.patches.clear();
                peer.live = protocol::LiveState {
                    agents: Vec::new(),
                    tasks: Vec::new(),
                    pending_patches: 0,
                };
                model.session_op = None;
                model.images.reset();
                model.backend_deferred = None;
                model.tool_viz = tool_viz(&bootstrap);
                model.restore.execution = bootstrap.execution_backend;
                model.memory_enabled = bootstrap.memory_enabled;
                restore_snapshot(model, snapshot);
            }
            Answer::Images(images) => {
                if let Effect::Images { generation, .. } = done.effect {
                    match model.images.accept(generation, images) {
                        Ok(notice) => {
                            model.push_notice(notice);
                            if let Some(draft) = model.backend_deferred.take() {
                                send_held(model, draft);
                            }
                        }
                        Err(error) => {
                            if let Some(draft) = model.backend_deferred.take() {
                                restore_draft(model, draft.raw);
                            }
                            model.push_notice(error);
                        }
                    }
                }
            }
            Answer::Reloaded(reloaded) => {
                for warning in reloaded.warnings {
                    model.push_notice(format!("Extension: {warning}"));
                }
                if let Effect::Reload(after) = done.effect {
                    after_reload(model, after);
                }
            }
        },
    }
    model.dirty = true;
}

fn tick(model: &mut Model) -> bool {
    backend_plugins::review_startup(model);
    let expired = model
        .clipboard_status
        .as_ref()
        .is_some_and(|(_, until)| Instant::now() >= *until);
    if expired {
        model.clipboard_status = None;
    }
    model.anim_frame = model.anim_frame.wrapping_add(1);
    if let Some(frame) = model.intro_frame {
        model.intro_frame = if frame >= 40 { None } else { Some(frame + 1) };
    }
    let poll = model.remote.as_ref().is_some_and(|peer| {
        !peer.live_pending && !peer.recovering && !peer.ended && peer.normal < NORMAL_REQUESTS
    });
    let frequency = if model.running
        || model.agent_runs.iter().any(|agent| agent.is_running())
        || model.bg_running() > 0
    {
        32
    } else {
        128
    };
    if model.anim_frame.is_multiple_of(frequency)
        && poll
        && request(model, Effect::Inspect(Inspect::Live, After::Notice))
    {
        model.remote.as_mut().expect("viewer").live_pending = true;
    }
    expired
}

pub(super) fn cancel_foreground(model: &mut Model) {
    if request(model, Effect::Cancel) {
        model.cancelling = true;
        model.push_main_notice("⏹ stopping — letting in-flight tools settle…");
    }
}
pub(super) fn force_abort_foreground_turn(model: &mut Model) {
    if request(model, Effect::Abort) {
        model.force_aborting = true;
        model.push_main_notice("Force stopping — waiting for owned work to settle…");
    }
}

pub(super) fn handle_approval_key(model: &mut Model, key: KeyEvent) {
    match key.code {
        KeyCode::PageUp => {
            model.scroll_by(-((model.viewport_height.max(2) / 2) as i32));
            return;
        }
        KeyCode::PageDown => {
            model.scroll_by((model.viewport_height.max(2) / 2) as i32);
            return;
        }
        KeyCode::Home => {
            model.scroll_to_top();
            return;
        }
        KeyCode::End => {
            model.scroll_to_bottom();
            return;
        }
        KeyCode::Char('d') => {
            model.approval_expanded = !model.approval_expanded;
            model.dirty = true;
            return;
        }
        _ => {}
    }
    if !model.approval_ready {
        return;
    }
    let Some(pending) = model.pending_approval() else {
        return;
    };
    let ApprovalResponder::Remote(prompt) = &pending.responder;
    let prompt = prompt.clone();
    let id = prompt.gate_id;
    let count = prompt.choices.len();
    if count == 0
        || model
            .remote
            .as_ref()
            .is_some_and(|peer| peer.approvals_answering.contains(&id))
    {
        return;
    }
    let choice = match key.code {
        KeyCode::Char('y' | 'Y' | '1') => Some(0),
        KeyCode::Char('a' | 'A' | '2') => Some(1),
        KeyCode::Char('3') => Some(2),
        KeyCode::Char('4') => Some(3),
        KeyCode::Char('n' | 'N') => Some(count - 1),
        KeyCode::Enter => Some(model.approval_sel),
        KeyCode::Up => {
            model.approval_sel = model.approval_sel.checked_sub(1).unwrap_or(count - 1);
            None
        }
        KeyCode::Down => {
            model.approval_sel = (model.approval_sel + 1) % count;
            None
        }
        _ => None,
    };
    if let Some(decision) = choice.and_then(|index| prompt.choices.get(index).copied())
        && request(
            model,
            Effect::Approve(protocol::AnswerApproval {
                gate_id: id,
                decision,
            }),
        )
    {
        model
            .remote
            .as_mut()
            .expect("viewer")
            .approvals_answering
            .insert(id);
    }
    model.dirty = true;
}

pub(super) fn submit_clarify(model: &mut Model) {
    if let Some(state) = model.clarify.as_mut()
        && let Some(index) = state
            .questions
            .iter()
            .enumerate()
            .find_map(|(index, question)| {
                let draft = &state.drafts[index];
                (!question.multi_select && draft.selected.is_empty() && draft.other.is_none())
                    .then_some(index)
            })
    {
        state.idx = index;
        state.cursor = 0;
        state.validation = Some("Choose one option or enter Other before submitting.".into());
        model.dirty = true;
        return;
    }
    respond_question(model, false);
}
pub(super) fn cancel_clarify(model: &mut Model) {
    respond_question(model, true);
}
fn respond_question(model: &mut Model, dismiss: bool) {
    let Some(state) = &model.clarify else {
        return;
    };
    let QuestionResponder::Remote(id) = &state.responder;
    let id = *id;
    if model
        .remote
        .as_ref()
        .is_some_and(|peer| peer.questions_answering.contains(&id))
    {
        return;
    }
    let answers = if dismiss {
        Vec::new()
    } else {
        state
            .answers()
            .into_iter()
            .map(|answer| protocol::Answer {
                selected: answer.selected,
                other: answer.other,
            })
            .collect()
    };
    if request(
        model,
        Effect::Question(protocol::AnswerQuestion {
            question_id: id,
            dismiss,
            answers,
        }),
    ) {
        model
            .remote
            .as_mut()
            .expect("viewer")
            .questions_answering
            .insert(id);
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
