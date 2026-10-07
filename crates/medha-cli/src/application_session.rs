//! Commands over the chat that owns the runtime. Frontends never open these
//! stores or mutate the conversation independently of its owner.
use crate::{acp::Writer, application_resources, desktop_extensions::Runtime};
use kernel::{EventLog, Kernel, Message, Provider, Session};
use serde_json::{Value, json};
use std::sync::Arc;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &Session,
    transcript: &mut Vec<Message>,
    resources: &Runtime,
    method: &str,
    params: Value,
    running: bool,
    writer: &Arc<Writer>,
    agents: Option<&Arc<orchestrator::AgentControl>>,
) -> Option<Result<Value, String>> {
    let result = match method {
        "session.resources" => match serde_json::from_value::<protocol::ChatResource>(params) {
            Ok(request) => {
                if matches!(&request.0, protocol::ReadResource::Pulse) {
                    let cfg = crate::config::load().map_err(|error| error.to_string());
                    return Some(cfg.map(|cfg| {
                        json!(protocol::ResourceResult::Pulse(protocol::ResourceNotice {
                            messages: vec![
                                crate::config::pulse_with(
                                    cfg.as_ref(),
                                    resources.flag_base_url.clone(),
                                    resources.flag_model.clone(),
                                    Some(resources.workspace.root()),
                                    &resources.model_env,
                                    Vec::new(),
                                    Vec::new()
                                )
                                .render()
                            ],
                        }))
                    }));
                }
                let known = kernel
                    .executor
                    .specs()
                    .into_iter()
                    .map(|spec| spec.name)
                    .collect();
                let result = application_resources::read(
                    kernel.log.as_ref(),
                    resources.workspace.root(),
                    request.0,
                    Some(&resources.skills),
                    Some(&resources.store),
                    &known,
                )
                .await;
                match result {
                    Ok(protocol::ResourceResult::Plugins(mut catalogue)) => {
                        if let Some(manager) = &resources.mcp {
                            let states = manager.status().await;
                            for plugin in &mut catalogue.plugins {
                                if plugin.activation != protocol::PluginActivation::Enabled {
                                    continue;
                                }
                                let mut rank = match plugin.health.as_deref() {
                                    Some("failed") => 3,
                                    Some("restarting") => 2,
                                    Some("starting") => 1,
                                    _ => 0,
                                };
                                for component in plugin
                                    .components
                                    .iter()
                                    .filter(|component| component.kind == "MCP server")
                                {
                                    let id =
                                        crate::plugin_session::server_id(&plugin.id, &component.id);
                                    let state = states.iter().find(|state| state.server == id);
                                    rank = rank.max(match state.map(|state| state.state) {
                                        Some(mcp::ServerState::Ready) => 0,
                                        Some(
                                            mcp::ServerState::Failed
                                            | mcp::ServerState::NeedsAuth
                                            | mcp::ServerState::NeedsToken,
                                        ) => 3,
                                        Some(
                                            mcp::ServerState::Reconnecting
                                            | mcp::ServerState::Degraded
                                            | mcp::ServerState::Parked,
                                        ) => 2,
                                        _ => 1,
                                    });
                                    if let Some(detail) =
                                        state.and_then(|state| state.detail.as_ref())
                                    {
                                        plugin.last_failure =
                                            Some(format!("{}: {detail}", component.id));
                                    }
                                }
                                plugin.health = Some(
                                    ["ready", "starting", "restarting", "failed"][rank].into(),
                                );
                            }
                        }
                        Ok(json!(protocol::ResourceResult::Plugins(catalogue)))
                    }
                    result => result
                        .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string())),
                }
            }
            Err(_) => Err("Invalid resource request".into()),
        },
        "application.skill.use" => match serde_json::from_value::<protocol::UseSkill>(params) {
            Ok(request) => {
                if running {
                    Err("Finish the current turn before loading a skill.".into())
                } else {
                    let known = kernel
                        .executor
                        .specs()
                        .into_iter()
                        .map(|spec| spec.name)
                        .collect();
                    match crate::skill_hub::loaded_message(&resources.skills, &request.name, &known)
                    {
                        Ok((description, message)) => {
                            transcript.push(Message::user(message.clone()));
                            let note = format!(
                                "Loaded skill '{}' — {description}. It is in context for the next turn.",
                                request.name
                            );
                            writer.notify_params(
                                "event",
                                &protocol::TurnEvent::Notice { text: note },
                            );
                            Ok(json!(protocol::ResourceResult::SkillUsed {
                                name: request.name,
                                description,
                                message,
                            }))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            Err(_) => Err("Invalid skill request".into()),
        },
        "application.command.expand" => {
            match serde_json::from_value::<protocol::ExpandCommand>(params) {
                Ok(request) => {
                    let builtin: Vec<_> = crate::application_catalog::COMMANDS
                        .iter()
                        .map(|(name, _)| *name)
                        .collect();
                    let commands = crate::application_commands::load(&resources.store, &builtin);
                    if crate::application_commands::has_snippets(&commands, &request.typed)
                        && (running
                            || agents.is_some_and(|control| !control.active().is_empty())
                            || kernel
                                .executor
                                .background_tasks()
                                .iter()
                                .any(|task| task.running))
                    {
                        return Some(Err(
                            "Finish active work before running plugin command snippets.".into(),
                        ));
                    }
                    crate::application_commands::expand_with_snippets(
                        &commands,
                        &request.typed,
                        &resources.store,
                        resources.workspace.root(),
                    )
                    .await
                    .and_then(|prompt| {
                        prompt.ok_or_else(|| "This command is no longer enabled.".into())
                    })
                    .map(|prompt| {
                        json!(protocol::ExpandedCommand {
                            typed: request.typed,
                            prompt
                        })
                    })
                }
                Err(_) => Err("Invalid command request".into()),
            }
        }
        "session.mcp" => match serde_json::from_value::<protocol::McpCommand>(params) {
            Ok(command) => crate::application_mcp::command(
                resources,
                command,
                writer,
                running
                    || agents.is_some_and(|control| !control.active().is_empty())
                    || !kernel.executor.background_tasks().is_empty(),
            )
            .await
            .map(|reply| json!(reply)),
            Err(_) => Err("Invalid MCP request".into()),
        },
        "session.inspect" => match serde_json::from_value::<protocol::SessionRead>(params) {
            Ok(query) => inspect(kernel, session, resources, agents, query)
                .await
                .map(|result| json!(result)),
            Err(_) => Err("Invalid session query".into()),
        },
        "session.change" => match serde_json::from_value::<protocol::SessionChange>(params) {
            Ok(change) => change_session(kernel, session, resources, agents, running, change)
                .await
                .map(|result| json!(result)),
            Err(_) => Err("Invalid session change".into()),
        },
        _ => return None,
    };
    Some(result)
}

fn typed<T: serde::de::DeserializeOwned>(value: impl serde::Serialize) -> Result<T, String> {
    serde_json::from_value(json!(value)).map_err(|_| "Invalid application projection".into())
}

async fn inspect<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &Session,
    resources: &Runtime,
    agents: Option<&Arc<orchestrator::AgentControl>>,
    query: protocol::SessionRead,
) -> Result<protocol::SessionResult, String> {
    use protocol::{SessionRead as Read, SessionResult as Reply};
    Ok(match query {
        Read::Bootstrap => Reply::Bootstrap(protocol::Bootstrap {
            tools: kernel
                .executor
                .specs()
                .into_iter()
                .map(|spec| protocol::ToolPresentation {
                    name: spec.name,
                    icon: spec.icon,
                    category: match spec.category {
                        kernel::ToolCategory::Read => "read",
                        kernel::ToolCategory::Write => "write",
                        kernel::ToolCategory::Shell => "shell",
                        kernel::ToolCategory::Search => "search",
                        kernel::ToolCategory::Web => "web",
                        kernel::ToolCategory::Vcs => "vcs",
                        kernel::ToolCategory::Diagnostic => "diagnostic",
                        kernel::ToolCategory::Plan => "plan",
                        kernel::ToolCategory::Other => "other",
                    }
                    .into(),
                })
                .collect(),
            show_thinking: resources.ui.show_thinking,
            full_transparency: resources.ui.full_transparency,
            execution_backend: resources.workspace.exec_backend_label().into(),
            memory_enabled: resources.memory.is_some(),
        }),
        Read::Live => {
            let progress = agents.map(|control| control.progress()).unwrap_or_default();
            let agent_states = agents
                .map(|control| control.agents())
                .unwrap_or_default()
                .into_iter()
                .map(|agent| {
                    let live = progress.get(&agent.path);
                    Ok(protocol::AgentState {
                        path: agent.path.to_string(),
                        session: agent.session,
                        objective: agent.objective,
                        started_ms: agent.started_ms,
                        write: agent.write,
                        status: match agent.state {
                            orchestrator::State::Running => None,
                            orchestrator::State::Settled(status) => Some(typed(status)?),
                        },
                        phase: live.map(|state| match &state.phase {
                            kernel::Phase::Generating => protocol::AgentPhase::Generating,
                            kernel::Phase::Idle => protocol::AgentPhase::Idle,
                            kernel::Phase::Settled => protocol::AgentPhase::Settled,
                            kernel::Phase::InTool { tool, target } => {
                                protocol::AgentPhase::InTool {
                                    tool: tool.clone(),
                                    target: target.clone(),
                                }
                            }
                            kernel::Phase::AwaitingApproval { action } => {
                                protocol::AgentPhase::AwaitingApproval {
                                    action: action.clone(),
                                }
                            }
                        }),
                        tool_calls: live.map_or(0, |state| u64::from(state.tool_calls)),
                        tokens: live.map_or(0, |state| state.tokens),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Reply::Live(protocol::LiveState {
                agents: agent_states,
                pending_patches: agents.map_or(0, |control| control.cached_unmerged()),
                tasks: kernel
                    .executor
                    .background_tasks()
                    .into_iter()
                    .map(|task| protocol::BackgroundTask {
                        id: task.id,
                        command: task.command,
                        running: task.running,
                    })
                    .collect(),
            })
        }
        Read::Lsp => {
            // This exact read-only status action is the API. There is no
            // frontend-controlled arbitrary executor dispatch.
            let observation = kernel
                .executor
                .execute(&kernel::ToolIntent {
                    id: "application-lsp-status".into(),
                    tool: "lsp".into(),
                    args: json!({"op":"status"}),
                })
                .await;
            if observation.status != kernel::ObsStatus::Ok {
                return Err(observation.payload["error"]
                    .as_str()
                    .unwrap_or("LSP is disabled")
                    .into());
            }
            Reply::Lsp(observation.payload)
        }
        Read::Usage => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_secs_f64())
                .unwrap_or_default();
            let (calls, sessions) =
                crate::usage_insights::collect(kernel.log.as_ref(), 7, now).await;
            let window = crate::usage_insights::summarize(&calls, &sessions, 7);
            let mine: Vec<_> = calls
                .iter()
                .filter(|call| {
                    call.session == session.id
                        || sessions.get(&call.session).and_then(|(_, parent)| *parent)
                            == Some(session.id)
                })
                .cloned()
                .collect();
            Reply::Text(crate::usage_insights::render(
                &crate::usage_insights::summarize(&mine, &sessions, 7),
                &window,
            ))
        }
        Read::Memories => {
            let memory = resources
                .memory
                .as_ref()
                .ok_or("Memory is off in this workspace")?;
            Reply::Memories(typed(
                memory.list_async().await.map_err(|e| e.to_string())?,
            )?)
        }
        Read::MemoryEvidence { scope, name } => {
            let memory = resources
                .memory
                .as_ref()
                .ok_or("Memory is off in this workspace")?;
            let entry = memory
                .get(typed(scope)?, &name)
                .map_err(|e| e.to_string())?
                .ok_or("That memory no longer exists")?;
            let evidence = crate::desktop_memory::provenance(
                kernel.log.as_ref(),
                Some(memory),
                &json!({"scope":scope,"name":name}),
            )
            .await?;
            Reply::MemoryEvidence(Box::new(protocol::MemoryEvidence {
                entry: typed(entry)?,
                session: evidence["session"].as_str().map(str::to_owned),
                event: evidence["event"].as_str().map(str::to_owned),
                kind: evidence["kind"].as_str().map(str::to_owned),
                timestamp: evidence["ts"].as_f64(),
                excerpt: evidence["excerpt"].as_str().map(str::to_owned),
            }))
        }
        Read::AgentTranscript { session: child } => {
            let control = agents.ok_or("Agents are not enabled")?;
            let child = control
                .address(&orchestrator::AgentPath::root(), &child)
                .map_err(|e| e.to_string())?;
            let id = child
                .session
                .parse()
                .map_err(|_| "Invalid agent conversation")?;
            let events = kernel
                .log
                .checked_events(id)
                .await
                .map_err(|e| e.to_string())?;
            let (items, omitted) =
                crate::chat_presentation::history(&kernel::project_messages(&events));
            Reply::Transcript { items, omitted }
        }
        Read::AgentPatches => {
            let mut rows = Vec::new();
            if let Some(control) = agents {
                for pending in control.outstanding().await {
                    rows.push(protocol::AgentPatch {
                        id: pending.dispatch,
                        agent: pending.agent,
                        session: pending.session,
                        files: pending.patch.files,
                        diff: pending.patch.diff,
                        verification: pending.patch.verification.map(typed).transpose()?,
                    });
                }
            }
            Reply::Patches(rows)
        }
    })
}

async fn change_session<P: Provider, L: EventLog>(
    kernel: &Kernel<P, L>,
    session: &Session,
    resources: &Runtime,
    agents: Option<&Arc<orchestrator::AgentControl>>,
    running: bool,
    change: protocol::SessionChange,
) -> Result<protocol::SessionResult, String> {
    use protocol::{SessionChange as Change, SessionResult as Reply};
    match change {
        Change::PinMemory {
            scope,
            name,
            pinned,
        } => {
            crate::desktop_memory::change(
                kernel.log.as_ref(),
                session,
                resources.memory.as_ref(),
                "memory.pin",
                &json!({"scope":scope,"name":name,"pinned":pinned}),
            )
            .await?;
            inspect(
                kernel,
                session,
                resources,
                agents,
                protocol::SessionRead::Memories,
            )
            .await
        }
        Change::ForgetMemory { scope, name } => {
            crate::desktop_memory::change(
                kernel.log.as_ref(),
                session,
                resources.memory.as_ref(),
                "memory.forget",
                &json!({"scope":scope,"name":name}),
            )
            .await?;
            inspect(
                kernel,
                session,
                resources,
                agents,
                protocol::SessionRead::Memories,
            )
            .await
        }
        Change::ApplyPatch {
            id,
            override_verification,
        } => {
            if running
                || agents.is_some_and(|control| !control.active().is_empty())
                || !kernel.executor.background_tasks().is_empty()
            {
                return Err("Finish or stop active work before applying a patch.".into());
            }
            let control = agents.ok_or("Agents are not enabled")?;
            let pending = control
                .pending(&id)
                .await
                .ok_or("That patch is no longer available")?;
            if pending.dispatch != id {
                return Err("Use the exact patch id from Changes".into());
            }
            control
                .merge(&pending.patch, override_verification)
                .await
                .map_err(|e| format!("Nothing applied: {e}"))?;
            control.forget(&id).await;
            Ok(Reply::Text(format!(
                "Applied {}{}",
                pending.patch.files.join(", "),
                if override_verification {
                    " (verification override requested)"
                } else {
                    ""
                }
            )))
        }
        Change::StopAgents => {
            if let Some(control) = agents {
                for agent in control.active() {
                    control
                        .cancel(&orchestrator::AgentPath::root(), agent.path.as_str())
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(Reply::Changed)
        }
    }
}
