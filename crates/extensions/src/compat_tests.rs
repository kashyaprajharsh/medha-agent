use super::*;

fn decode(point: HookPoint, status: i32, stdout: &str, stderr: &str) -> HookResult {
    decode_output(point, Some(status), stdout.as_bytes(), stderr.as_bytes()).unwrap()
}

#[test]
fn tool_input_uses_the_existing_names_and_shapes() {
    let input = encode_input(
        HookPoint::PreTool,
        "s1",
        Path::new("/work"),
        &json!({"tool": "shell.exec", "args": {"command": "git commit -m x"}}),
    );
    assert_eq!(input["hook_event_name"], "PreToolUse");
    assert_eq!(input["tool_name"], "Bash");
    assert_eq!(input["tool_input"]["command"], "git commit -m x");
    assert_eq!(input["cwd"], "/work");
    assert_eq!(canonical_tool_name("Bash"), "shell.exec");
    assert_eq!(
        canonical_tool_name("mcp__github__search"),
        "mcp__github__search"
    );
}

#[test]
fn settings_hooks_become_sandboxed_packages_approved_by_hooks_and_scripts_only() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".claude");
    std::fs::create_dir_all(dir.join("hooks")).unwrap();
    std::fs::write(dir.join("hooks/check.py"), "print()").unwrap();
    let settings = |allow: &str| {
        json!({
            "permissions": {"allow": [allow]},
            "hooks": {
                "PreToolUse": [{"matcher": "Bash|Edit", "hooks": [
                    {"type": "command", "command": "python3 \"$CLAUDE_PROJECT_DIR/.claude/hooks/check.py\"", "timeout": 5}
                ]}],
                "Notification": [{"hooks": [{"type": "command", "command": "say hi"}]}],
                "Stop": [{"hooks": [{"type": "prompt", "prompt": "done?"}]}]
            }
        })
        .to_string()
    };
    std::fs::write(dir.join("settings.local.json"), settings("Bash(ls)")).unwrap();
    let package = load_settings(&dir, PROJECT_SETTINGS_HOOKS_ID, "0.1.8")
        .unwrap()
        .unwrap();
    let ExtensionComponent::Hook {
        points,
        matcher,
        entrypoint,
        protocol,
        workdir,
        timeout_ms,
        ..
    } = &package.manifest.components[0]
    else {
        panic!("expected a hook");
    };
    assert_eq!(points, &[HookPoint::PreTool]);
    assert_eq!(matcher, &["Bash", "Edit"]);
    assert!(entrypoint.shell);
    assert_eq!(*protocol, HookProtocol::ExitStatus);
    assert_eq!(*workdir, HookWorkdir::Workspace);
    assert_eq!(*timeout_ms, 5_000);
    assert_eq!(package.manifest.components.len(), 1);
    let description = package.manifest.description.clone().unwrap();
    assert!(description.contains("Notification"), "{description}");
    assert!(description.contains("Stop (prompt hook)"), "{description}");

    std::fs::write(
        dir.join("settings.local.json"),
        settings("Bash(git status)"),
    )
    .unwrap();
    let reloaded = package.reload("0.1.8").unwrap();
    assert_eq!(
        reloaded.content_hash, package.content_hash,
        "permission edits do not revoke approved hooks"
    );
    std::fs::write(dir.join("hooks/check.py"), "print('changed')").unwrap();
    assert_ne!(
        package.reload("0.1.8").unwrap().content_hash,
        package.content_hash
    );
}

#[test]
fn exit_status_and_json_map_to_narrowing_decisions() {
    assert_eq!(
        decode(HookPoint::PreTool, 0, "", "").decision,
        HookDecision::Continue
    );
    let blocked = decode(HookPoint::PreTool, 2, "", "no force pushes");
    assert_eq!(blocked.decision, HookDecision::Deny);
    assert_eq!(blocked.reason.as_deref(), Some("no force pushes"));

    let denied = decode(
        HookPoint::PreTool,
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"secret file"}}"#,
        "",
    );
    assert_eq!(denied.decision, HookDecision::Deny);
    let ask = decode(
        HookPoint::PreTool,
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
        "",
    );
    assert_eq!(ask.decision, HookDecision::RequestApproval);
    let allow = decode(
        HookPoint::PreTool,
        0,
        r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
        "",
    );
    assert_eq!(
        allow.decision,
        HookDecision::Continue,
        "allow never widens policy"
    );

    let feedback = decode(
        HookPoint::TaskCompletion,
        0,
        r#"{"decision":"block","reason":"tests are failing"}"#,
        "",
    );
    assert_eq!(feedback.decision, HookDecision::AddContext);
    assert_eq!(feedback.context.as_deref(), Some("tests are failing"));

    let message = decode(
        HookPoint::PostTool,
        0,
        r#"{"systemMessage":"Em dashes stripped from: a.md"}"#,
        "",
    );
    assert_eq!(message.decision, HookDecision::Annotate);
    assert!(
        decode_output(HookPoint::PreTool, Some(1), b"", b"boom")
            .unwrap_err()
            .contains("boom")
    );
}

#[test]
fn a_catalog_entry_defines_a_folder_without_its_own_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("kit");
    for skill in ["local-ai", "nested/serve"] {
        std::fs::create_dir_all(dir.join(skill)).unwrap();
        std::fs::write(
            dir.join(skill).join("SKILL.md"),
            "---\nname: s\ndescription: d\n---\nsteps\n",
        )
        .unwrap();
    }
    let listing = crate::sources::Listing {
        name: "kit".into(),
        description: "AMD skills".into(),
        category: None,
        source: crate::sources::GitSource {
            url: "https://example.com/kit".into(),
            git_ref: None,
            subdir: None,
        },
        entry: Some(
            json!({"version": "1.2.0", "skills": ["./local-ai", "./nested", "../outside"]})
                .to_string(),
        ),
    };
    apply_entry(&dir, &entry_for(&listing)).unwrap();
    let package = load_plugin(&dir, Some("market.kit"), "0.1.8").unwrap();
    assert_eq!(package.manifest.version, "1.2.0");
    assert_eq!(package.manifest.description.as_deref(), Some("AMD skills"));
    let skills: Vec<&str> = package
        .manifest
        .components
        .iter()
        .map(|component| component.id())
        .collect();
    assert_eq!(skills, ["local-ai", "serve"]);
}

#[test]
fn converted_hooks_are_named_after_their_scripts() {
    let hooks = json!({
        "SessionStart": [{"hooks": [
            {"type": "command", "command": "python3 \"${CLAUDE_PLUGIN_ROOT}/hooks/session-start.py\""},
            {"type": "command", "command": "echo hi"}
        ]}],
        "PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "node hooks/guard.js"},
            {"type": "command", "command": "node other/guard.js"}
        ]}]
    });
    let mut components = Vec::new();
    convert_section(&hooks, &mut components, &mut Vec::new());
    let mut ids: Vec<&str> = components.iter().map(|component| component.id()).collect();
    ids.sort();
    assert_eq!(
        ids,
        [
            "pre-tool-guard",
            "pre-tool-guard-2",
            "session-start",
            "session-start-2"
        ]
    );
}
