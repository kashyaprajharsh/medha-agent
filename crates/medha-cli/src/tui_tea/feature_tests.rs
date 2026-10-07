use super::*;

fn plugin(id: &str, network: bool) -> protocol::PluginSummary {
    protocol::PluginSummary {
        id: id.into(),
        version: "1".into(),
        scope: protocol::ResourceScope::Project,
        activation: protocol::PluginActivation::Disabled,
        hash: "reviewed-content-hash".into(),
        root: PathBuf::from("/unused"),
        components: Vec::new(),
        actions: Vec::new(),
        updatable: false,
        can_rollback: false,
        health: None,
        last_failure: None,
        grant: Some(protocol::Grant {
            network,
            ..Default::default()
        }),
        blocked: None,
        access_description: "the reviewed access".into(),
        hooks: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attached_terminal_draws_running_agents_from_the_snapshot_before_polling() {
    let mut f = fixture().await;
    let mut snap = snapshot(&f.model, "current");
    snap.roster.push(protocol::RosterAgent {
        name: "worker".into(),
        path: "/worker".into(),
        session: "child".into(),
        write: false,
        objective: "read".into(),
        started_ms: 1,
        status: "running".into(),
        doing: Some(protocol::AgentDoing::Thinking),
        tool_calls: Some(2),
        tokens: Some(10),
    });
    restore_snapshot(&mut f.model, snap);
    assert_eq!(f.model.agent_runs.len(), 1);
    assert_eq!(f.model.agent_runs[0].session, "child");
    assert!(f.model.has_active_agents());
    assert_eq!(
        f.model.agent_progress.values().next().unwrap().tool_calls,
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_access_review_sends_only_the_confirmed_hash_and_keeps_the_selected_row() {
    let mut f = fixture().await;
    let rows = vec![plugin("first", false), plugin("second", true)];
    backend_plugins::catalogue(
        &mut f.model,
        protocol::PluginCatalogue {
            plugins: rows.clone(),
            notices: Vec::new(),
            commands: Vec::new(),
            hook_review: None,
        },
        After::PluginsAt("second".into()),
    );
    assert_eq!(f.model.picker.as_ref().unwrap().selected, 3);
    backend_plugins::handle_key(&mut f.model, KeyCode::Char(' '));
    assert!(
        f.model.remote.as_ref().unwrap().pending.is_empty(),
        "opening access review must not approve"
    );
    backend_plugins::handle_key(&mut f.model, KeyCode::Down);
    backend_plugins::handle_key(&mut f.model, KeyCode::Enter);
    let Pending {
        effect:
            Effect::Change(
                protocol::ChangeResource::EnablePlugin {
                    id, hash, grant, ..
                },
                _,
            ),
        ..
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap()
    else {
        panic!("no approval command");
    };
    assert_eq!(id, "second");
    assert_eq!(hash, "reviewed-content-hash");
    assert!(grant.network);
    assert_eq!(f.model.input, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_hook_review_waits_for_forms_and_escape_leaves_hooks_disabled() {
    let mut f = fixture().await;
    f.model.input = "composer draft".into();
    begin_model_setup(&mut f.model);
    backend_plugins::catalogue(
        &mut f.model,
        protocol::PluginCatalogue {
            plugins: Vec::new(),
            notices: Vec::new(),
            commands: Vec::new(),
            hook_review: Some(plugin("project-hooks", false)),
        },
        After::Startup,
    );
    assert!(!matches!(
        &f.model.picker.as_ref().unwrap().kind,
        PickerKind::BackendPlugins(_)
    ));
    assert!(
        f.model
            .remote
            .as_ref()
            .unwrap()
            .plugins
            .hook_review
            .is_some()
    );
    f.model.model_setup = None;
    f.model.picker = None;
    backend_plugins::review_startup(&mut f.model);
    assert!(
        matches!(&f.model.picker.as_ref().unwrap().kind, PickerKind::BackendPlugins(screen)
        if matches!(screen.as_ref(), backend_plugins::Screen::Grant { review: true, .. }))
    );
    backend_plugins::handle_key(&mut f.model, KeyCode::Esc);
    backend_plugins::review_startup(&mut f.model);
    assert!(f.model.picker.is_none());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    assert_eq!(f.model.input, "composer draft");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presentation_recovery_cannot_hide_an_in_flight_agent_launch() {
    let mut f = fixture().await;
    assert!(request(
        &mut f.model,
        Effect::Agent(
            protocol::AgentCommand::Followup {
                agent: "worker".into(),
                text: "continue".into(),
            },
            None
        )
    ));
    let snap = snapshot(&f.model, "recovered");
    restore_snapshot(&mut f.model, snap);
    assert!(f.model.has_active_agents());
    assert_eq!(f.model.pending_agent_launches, 1);
    receipt(&mut f, true);
    assert!(!f.model.has_active_agents());
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn receipt(f: &mut Fixture, accepted: bool) {
    let Pending {
        effect,
        form,
        command,
    } = f
        .model
        .remote
        .as_mut()
        .unwrap()
        .pending
        .pop_front()
        .unwrap();
    completed(
        &mut f.model,
        &mut f.view,
        Completion {
            generation: 1,
            effect,
            form,
            command,
            result: Ok(Answer::Accepted(protocol::Accepted { accepted })),
        },
    );
}

fn access(model: &mut Model) {
    approval(
        model,
        protocol::ApprovalPrompt {
            gate_id: 7,
            action: "Network access".into(),
            detail: Some("Review this command before allowing access.".into()),
            escalated: false,
            kind: protocol::ApprovalKind::Access,
            choices: vec![
                protocol::ApprovalDecision::Once,
                protocol::ApprovalDecision::Session,
                protocol::ApprovalDecision::Persistent,
                protocol::ApprovalDecision::Deny,
            ],
            folder: None,
            path: None,
        },
    );
    model.approval_ready = true;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_approval_keeps_all_four_choices_and_wraps_in_both_directions() {
    for (code, expected) in [
        (KeyCode::Char('1'), protocol::ApprovalDecision::Once),
        (KeyCode::Char('2'), protocol::ApprovalDecision::Session),
        (KeyCode::Char('3'), protocol::ApprovalDecision::Persistent),
        (KeyCode::Char('4'), protocol::ApprovalDecision::Deny),
        (KeyCode::Char('n'), protocol::ApprovalDecision::Deny),
    ] {
        let mut f = fixture().await;
        access(&mut f.model);
        handle_approval_key(&mut f.model, key(KeyCode::Up));
        assert_eq!(f.model.approval_sel, 3);
        handle_approval_key(&mut f.model, key(KeyCode::Down));
        assert_eq!(f.model.approval_sel, 0);
        handle_approval_key(&mut f.model, key(code));
        assert!(
            matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Approve(answer) if serde_json::to_value(answer.decision).unwrap() == serde_json::to_value(expected).unwrap())
        );
        assert_eq!(
            f.model.pending_approvals.len(),
            1,
            "a key is not an acknowledgement"
        );
        receipt(&mut f, true);
        assert!(f.model.pending_approvals.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_approval_labels_match_the_reviewed_scope_and_send_each_offered_answer() {
    use protocol::{ApprovalDecision as D, PathKind as K};
    for (kind, label, folder) in [
        (
            K::File,
            "Yes, always allow this file",
            Some("/outside/project"),
        ),
        (
            K::Directory,
            "Yes, always allow this folder and its contents",
            None,
        ),
        (
            K::Unknown,
            "Yes, always allow this path",
            Some("/outside/project"),
        ),
    ] {
        let mut choices = vec![D::Once, D::Always];
        if folder.is_some() {
            choices.push(D::Folder);
        }
        choices.push(D::Deny);
        for (index, expected) in choices.iter().copied().enumerate() {
            let mut f = fixture().await;
            let prompt = protocol::ApprovalPrompt {
                gate_id: 9,
                action: "Write access to /outside/project/target".into(),
                detail: Some("Remembering access applies to future chats in this project. Medha's file tools and commands can use it.".into()),
                escalated: false,
                kind: protocol::ApprovalKind::Path,
                choices: choices.clone(),
                folder: folder.map(str::to_owned),
                path: Some(protocol::ApprovalPath {
                    path: "/outside/project/target".into(), kind, access: protocol::PathAccess::Write,
                }),
            };
            observed(
                &mut f.model,
                medha_client::Said::Frame(serde_json::json!({
                    "method": "approval", "params": prompt,
                })),
            )
            .unwrap();
            f.model.approval_ready = true;
            let labels = f.model.pending_approvals[0].responder.options_for(false);
            assert_eq!(labels[1], label);
            assert_eq!(labels.len(), choices.len());
            handle_approval_key(
                &mut f.model,
                key(KeyCode::Char(char::from(b'1' + index as u8))),
            );
            assert!(
                matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Approve(answer) if serde_json::to_value(answer.decision).unwrap() == serde_json::to_value(expected).unwrap())
            );
            receipt(&mut f, true);
            assert!(f.model.pending_approvals.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_approval_remains_readable_and_review_never_approves_off_screen() {
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(32, 12), (80, 24), (120, 36)] {
        let mut f = fixture().await;
        access(&mut f.model);
        f.model.model = "a-provider-model-name-longer-than-the-terminal".into();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view(frame, &mut f.model)).unwrap();
        let text = (0..height)
            .map(|row| {
                (0..width)
                    .map(|col| terminal.backend().buffer()[(col, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Approval needed") || text.contains("waiting for your approval"),
            "{text}"
        );
        assert!(f.model.approval_ready);
    }
    let mut f = fixture().await;
    access(&mut f.model);
    f.model.pending_approvals[0].detail = Some(
        (0..40)
            .map(|row| format!("command evidence {row}\n"))
            .collect(),
    );
    let mut terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_transcript(frame, &mut f.model, area);
        })
        .unwrap();
    handle_approval_key(&mut f.model, key(KeyCode::Char('d')));
    handle_approval_key(&mut f.model, key(KeyCode::PageUp));
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_transcript(frame, &mut f.model, area);
        })
        .unwrap();
    assert!(!f.model.approval_ready);
    assert!(f.model.approval_expanded);
    handle_approval_key(&mut f.model, key(KeyCode::Enter));
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    handle_approval_key(&mut f.model, key(KeyCode::End));
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_transcript(frame, &mut f.model, area);
        })
        .unwrap();
    assert!(f.model.approval_ready);
    handle_approval_key(&mut f.model, key(KeyCode::Char('1')));
    assert!(matches!(
        f.model.remote.as_ref().unwrap().pending[0].effect,
        Effect::Approve(_)
    ));
}

fn ask(model: &mut Model, multi: bool, recommended: bool) {
    question(
        model,
        protocol::QuestionPrompt {
            question_id: 11,
            questions: vec![protocol::Question {
                header: "DB".into(),
                prompt: "Which DB?".into(),
                multi_select: multi,
                options: vec![
                    protocol::QuestionOption {
                        label: "Postgres".into(),
                        description: String::new(),
                        recommended,
                    },
                    protocol::QuestionOption {
                        label: "SQLite".into(),
                        description: String::new(),
                        recommended: false,
                    },
                ],
            }],
        },
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_radio_multi_other_and_dismiss_keep_their_original_semantics() {
    let mut f = fixture().await;
    ask(&mut f.model, false, true);
    input::handle_clarify_key(&mut f.model, key(KeyCode::Enter));
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Question(answer) if answer.answers[0].selected == ["Postgres"])
    );
    receipt(&mut f, true);
    ask(&mut f.model, false, true);
    f.model.clarify.as_mut().unwrap().cursor = 1;
    input::handle_clarify_key(&mut f.model, key(KeyCode::Enter));
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Question(answer) if answer.answers[0].selected == ["SQLite"])
    );
    receipt(&mut f, true);
    ask(&mut f.model, true, false);
    for code in [
        KeyCode::Char(' '),
        KeyCode::Down,
        KeyCode::Char(' '),
        KeyCode::Enter,
    ] {
        input::handle_clarify_key(&mut f.model, key(code));
    }
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Question(answer) if answer.answers[0].selected == ["Postgres", "SQLite"])
    );
    receipt(&mut f, true);
    ask(&mut f.model, false, false);
    submit_clarify(&mut f.model);
    assert!(f.model.clarify.as_ref().unwrap().validation.is_some());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    f.model.input = "keep this steer".into();
    let state = f.model.clarify.as_mut().unwrap();
    state.cursor = state.other_row();
    for code in [
        KeyCode::Char(' '),
        KeyCode::Char('é'),
        KeyCode::Char('🙂'),
        KeyCode::Enter,
    ] {
        input::handle_clarify_key(&mut f.model, key(code));
    }
    assert_eq!(
        f.model.clarify.as_ref().unwrap().drafts[0].other.as_deref(),
        Some("é🙂")
    );
    assert_eq!(f.model.input, "keep this steer");
    input::handle_clarify_key(&mut f.model, key(KeyCode::Esc));
    assert!(
        matches!(&f.model.remote.as_ref().unwrap().pending[0].effect, Effect::Question(answer) if answer.dismiss && answer.answers.is_empty())
    );
    receipt(&mut f, true);
    assert!(f.model.clarify.is_none());
    assert_eq!(f.model.input, "keep this steer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escape_prioritizes_visible_modal_then_graceful_stop_then_force_stop() {
    let mut f = fixture().await;
    f.model.running = true;
    backend_features::dispatch_slash(&mut f.model, "reasoning");
    assert!(f.model.picker.is_some());
    input::handle_key(&mut f.model, key(KeyCode::Esc));
    assert!(f.model.picker.is_none());
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());
    input::handle_key(&mut f.model, key(KeyCode::Esc));
    assert!(f.model.cancelling && f.model.running);
    assert!(matches!(
        f.model.remote.as_ref().unwrap().pending[0].effect,
        Effect::Cancel
    ));
    input::handle_key(&mut f.model, key(KeyCode::Esc));
    assert!(f.model.force_aborting && f.model.running);
    assert!(matches!(
        f.model.remote.as_ref().unwrap().pending[1].effect,
        Effect::Abort
    ));
    input::handle_key(
        &mut f.model,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );
    assert!(f.model.should_quit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_receipts_and_steps_keep_session_changes_owned_without_losing_rejected_text() {
    let mut f = fixture().await;
    let effect = Effect::Agent(
        protocol::AgentCommand::Followup {
            agent: "/writer".into(),
            text: "more".into(),
        },
        Some("/agents followup /writer more".into()),
    );
    assert!(request(&mut f.model, effect));
    assert!(f.model.has_active_agents());
    backend_features::dispatch_slash(&mut f.model, "clear");
    assert!(f.model.session_op.is_none());
    assert_eq!(f.model.remote.as_ref().unwrap().pending.len(), 1);
    receipt(&mut f, false);
    assert!(!f.model.has_active_agents());
    assert_eq!(f.model.input, "/agents followup /writer more");
    assert!(request(
        &mut f.model,
        Effect::Agent(
            protocol::AgentCommand::Steer {
                agent: "/writer".into(),
                text: "adjust".into()
            },
            None
        )
    ));
    f.model.push_agent_step(
        orchestrator::AgentPath::parse("/writer").unwrap(),
        AgentStep::Steered("adjust".into()),
    );
    receipt(&mut f, true);
    assert_eq!(
        f.model.pending_agent_steers, 0,
        "an event before its receipt must not resurrect pending work"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_command_suggestion_preserves_the_new_composer() {
    let mut f = fixture().await;
    f.model.input = "new work typed during discovery".into();
    backend_features::prefill_command(
        &mut f.model,
        "/skill update --all",
        "Enter to apply updates",
    );
    assert_eq!(f.model.input, "new work typed during discovery");
    assert!(f.model.items.iter().any(
        |entry| matches!(&entry.item, Item::Notice(text) if text.contains("/skill update --all"))
    ));
}

/// A sign-in to an MCP server is told in three parts, each as the backend
/// sends it: the answer to starting it, its link, and how it ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_sign_in_shows_its_link_and_how_it_ended_in_words() {
    let mut f = fixture().await;
    let last = |model: &Model| {
        let mut notices = model.items.iter().filter_map(|entry| match &entry.item {
            Item::Notice(text) => Some(text.clone()),
            _ => None,
        });
        notices.next_back().unwrap_or_default()
    };
    let said =
        |method: &str, params: Value| Said::Frame(json!({ "method": method, "params": params }));

    let begun = json!({ "server": "linear", "state": "signing_in" });
    backend_features::mcp_reply(
        &mut f.model,
        protocol::McpResult::Status(begun),
        After::Notice,
    );
    let shown = last(&f.model);
    assert!(shown.contains("linear [signing_in]"), "{shown}");
    assert!(!shown.contains('{'), "shown as it travels: {shown}");

    let link = json!({ "server": "linear", "url": "https://example.test/sign-in" });
    super::super::observed(&mut f.model, said("mcp.auth", link)).unwrap();
    let shown = last(&f.model);
    assert!(
        shown.contains("'linear'") && shown.contains("https://example.test/sign-in"),
        "{shown}"
    );

    let status = json!({ "server": "linear", "state": "ready", "tools": 12 });
    let done = json!({ "server": "linear", "ok": true, "status": status });
    super::super::observed(&mut f.model, said("mcp.signed_in", done)).unwrap();
    let shown = last(&f.model);
    assert!(shown.contains("linear [ready]  12 tool(s)"), "{shown}");

    let failed = json!({ "server": "linear", "ok": false, "error": "sign-in was cancelled" });
    super::super::observed(&mut f.model, said("mcp.signed_in", failed)).unwrap();
    let shown = last(&f.model);
    assert!(shown.contains("'linear': sign-in was cancelled"), "{shown}");
}

/// A skill whose code is to be read first is installed switched off. It must
/// still be found in the list, or there is no way here to switch it on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skill_left_off_at_install_is_listed_and_switched_on_only_by_saying_so() {
    let mut f = fixture().await;
    let skill = |name: &str, enabled: bool| protocol::SkillSummary {
        name: name.into(),
        description: "does a thing".into(),
        scope: "user".into(),
        available: enabled,
        enabled,
        missing_tools: Vec::new(),
        verdict: None,
    };
    let catalogue = protocol::SkillCatalogue {
        skills: vec![skill("ready", true), skill("flagged", false)],
        errors: Vec::new(),
    };
    backend_features::resource_reply(&mut f.model, Resource::Skills(catalogue), After::Skills);
    let rows = f.model.picker.as_ref().unwrap().kind.labels();
    let flagged = rows.iter().position(|row| row.starts_with("flagged"));
    let flagged = flagged.expect("a skill installed switched off was left out of the list");
    assert!(rows[flagged].contains("(disabled)"), "{}", rows[flagged]);

    // Picking it asks first, and the answer it starts on keeps it off.
    f.model.picker.as_mut().unwrap().selected = flagged;
    input::handle_key(&mut f.model, key(KeyCode::Enter));
    let asking = f.model.picker.as_ref().expect("nothing was asked");
    assert!(matches!(&asking.kind, PickerKind::EnableSkill(name) if name == "flagged"));
    assert_eq!(asking.selected, 0);
    assert!(f.model.remote.as_ref().unwrap().pending.is_empty());

    input::handle_key(&mut f.model, key(KeyCode::Down));
    input::handle_key(&mut f.model, key(KeyCode::Enter));
    let sent = &f.model.remote.as_ref().unwrap().pending;
    assert!(sent.iter().any(|asked| matches!(
        &asked.effect,
        Effect::Change(protocol::ChangeResource::ConfigureSkill { name, enabled: true }, _)
            if name == "flagged"
    )));
}

/// Space in the list switches a skill off or on. Only switching on one that
/// the safety check flagged is asked about first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_switches_a_skill_off_or_on_and_asks_only_for_one_that_was_flagged() {
    let skill = |name: &str, enabled: bool, verdict: Option<&str>| protocol::SkillSummary {
        name: name.into(),
        description: "does a thing".into(),
        scope: "user".into(),
        available: enabled,
        enabled,
        missing_tools: Vec::new(),
        verdict: verdict.map(str::to_owned),
    };
    let catalogue = protocol::SkillCatalogue {
        skills: vec![
            skill("ready", true, Some("safe")),
            skill("parked", false, Some("safe")),
            skill("flagged", false, Some("caution")),
        ],
        errors: Vec::new(),
    };
    // What pressing Space on one skill sends, and what it asks.
    async fn pressed(
        catalogue: &protocol::SkillCatalogue,
        on: &str,
    ) -> (Option<bool>, Option<String>) {
        let mut f = fixture().await;
        let listed = Resource::Skills(catalogue.clone());
        backend_features::resource_reply(&mut f.model, listed, After::Skills);
        let rows = f.model.picker.as_ref().unwrap().kind.labels();
        let row = rows.iter().position(|row| row.starts_with(on)).unwrap();
        f.model.picker.as_mut().unwrap().selected = row;
        input::handle_key(&mut f.model, key(KeyCode::Char(' ')));
        let sent = f.model.remote.as_ref().unwrap().pending.iter();
        let mut sent = sent.filter_map(|asked| match &asked.effect {
            Effect::Change(protocol::ChangeResource::ConfigureSkill { name, enabled }, _)
                if name == on =>
            {
                Some(*enabled)
            }
            _ => None,
        });
        let asked = f
            .model
            .picker
            .as_ref()
            .and_then(|picker| match &picker.kind {
                PickerKind::EnableSkill(name) => Some(name.clone()),
                _ => None,
            });
        (sent.next_back(), asked)
    }

    assert_eq!(pressed(&catalogue, "ready").await, (Some(false), None));
    assert_eq!(pressed(&catalogue, "parked").await, (Some(true), None));
    let flagged = pressed(&catalogue, "flagged").await;
    assert_eq!(flagged, (None, Some("flagged".into())));
}
