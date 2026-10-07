use super::*;
use serde_json::json;

#[test]
fn path_approvals_preserve_the_resolved_target_and_one_set_of_choices() {
    for kind in [PathKind::File, PathKind::Directory, PathKind::Unknown] {
        let prompt = ApprovalPrompt {
            gate_id: 4,
            action: "Read access to /outside/project/src".into(),
            detail: Some("Remembered folder access includes its contents and future chats.".into()),
            escalated: false,
            kind: ApprovalKind::Path,
            choices: vec![
                ApprovalDecision::Once,
                ApprovalDecision::Always,
                ApprovalDecision::Folder,
                ApprovalDecision::Deny,
            ],
            folder: Some("/outside/project".into()),
            path: Some(ApprovalPath {
                path: "/outside/project/src".into(),
                kind,
                access: PathAccess::Read,
            }),
        };
        let value = serde_json::to_value(&prompt).unwrap();
        let current: ApprovalPrompt = serde_json::from_value(value).unwrap();
        let target = current.path.unwrap();
        assert_eq!(target.kind, kind);
        assert_eq!(target.access, PathAccess::Read);
        assert_eq!(target.path, "/outside/project/src");
        assert!(matches!(
            current.choices.as_slice(),
            [
                ApprovalDecision::Once,
                ApprovalDecision::Always,
                ApprovalDecision::Folder,
                ApprovalDecision::Deny
            ]
        ));
    }
}

#[test]
fn approvals_without_path_metadata_keep_their_existing_choices() {
    let prompt: ApprovalPrompt = serde_json::from_value(json!({
        "gate_id": 8, "action": "command", "detail": null, "escalated": false,
        "kind": "access", "choices": ["once", "session", "persistent", "deny"],
    }))
    .unwrap();
    assert!(prompt.path.is_none());
    assert!(matches!(
        prompt.choices.as_slice(),
        [
            ApprovalDecision::Once,
            ApprovalDecision::Session,
            ApprovalDecision::Persistent,
            ApprovalDecision::Deny
        ]
    ));
}

#[test]
fn scoped_commands_match_the_existing_envelope_and_cannot_override_it() {
    let call = Call::new(Scope::Chat("live"), &Configure::Mode(Mode::Plan)).unwrap();
    assert_eq!(
        serde_json::to_value(call).unwrap(),
        json!({
            "jsonrpc": "2.0", "method": "session.configure",
            "session": "live", "params": {"mode": "plan"},
        })
    );
    assert!(Call::new(Scope::Service, &Cancel {}).is_err());
    assert!(Call::new(Scope::Folder(Path::new(".")), &GetSettings {}).is_err());
}

#[test]
fn changing_multiple_settings_or_sending_the_wrong_type_is_refused() {
    for wrong in [
        json!({"mode": "plan", "streaming": false}),
        json!({"mode": "typo"}),
        json!({"streaming": "yes"}),
        json!({"shell": "touch bad"}),
        json!({}),
    ] {
        assert!(serde_json::from_value::<Configure>(wrong).is_err());
    }
}

#[test]
fn future_events_are_distinct_from_malformed_known_events() {
    assert!(matches!(
        serde_json::from_value::<TurnEvent>(json!({
            "kind": "future.notice", "extra": 1,
        }))
        .unwrap(),
        TurnEvent::Unknown
    ));
    assert!(
        serde_json::from_value::<TurnEvent>(json!({
            "kind": "model.text", "delta": false,
        }))
        .is_err()
    );
    let result = serde_json::from_value::<TurnEvent>(json!({
        "kind": "tool.observation", "id": "call", "tool": "read",
        "ok": true, "payload": {"text": "hello"},
    }))
    .unwrap();
    assert!(matches!(result, TurnEvent::ToolResult { id: Some(id), .. } if id == "call"));
}

#[test]
fn an_unreported_cache_bucket_does_not_become_zero() {
    for cached in [None, Some(0), Some(5)] {
        let event = TurnEvent::Usage {
            prompt_tokens: 10,
            total_tokens: 20,
            completion_tokens: Some(10),
            cached_prompt_tokens: cached,
        };
        let decoded: TurnEvent =
            serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap();
        assert!(
            matches!(decoded, TurnEvent::Usage { cached_prompt_tokens, .. } if cached_prompt_tokens == cached)
        );
    }
    let legacy = serde_json::from_value::<TurnEvent>(json!({
        "kind": "usage", "prompt_tokens": 10, "total_tokens": 20,
    }))
    .unwrap();
    assert!(matches!(
        legacy,
        TurnEvent::Usage {
            cached_prompt_tokens: None,
            ..
        }
    ));
}

#[test]
fn old_settings_preserve_unknown_image_capabilities() {
    let settings: Settings = serde_json::from_value(json!({
        "profiles": [], "profile": "saved", "model": "model", "mode": "careful",
        "reasoning": "auto", "effort": "auto", "efforts": [],
        "reasoning_support": "unverified", "streaming": true, "context_limit": null,
    }))
    .unwrap();
    assert!(settings.protocol.is_none());
    assert!(settings.image_input.is_none());
    assert!(settings.image_support.is_none());
    assert_eq!(settings.reasoning_support, ReasoningSupport::Unverified);
}

#[test]
fn resource_commands_refuse_invented_actions_and_redact_secret_debugging() {
    for wrong in [
        json!({"action":"execute","data":{"shell":"touch bad"}}),
        json!({"action":"remove_model","data":{"name":"x","extra":true}}),
        json!({"action":"search","data":{"provider":"wrong","key":"secret","url":null}}),
    ] {
        assert!(serde_json::from_value::<ChangeResource>(wrong).is_err());
    }
    assert!(
        serde_json::from_value::<SessionRead>(json!({"query":"lsp","args":{"op":"execute"}}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<McpCommand>(
            json!({"operation":"credential", "data":{"id":"x","key":"k","header":"made-up"}})
        )
        .is_err()
    );
    let secret = McpCommand::Add {
        definition: Secret("server https://example.test --key sk-private".into()),
    };
    assert!(!format!("{secret:?}").contains("sk-private"));
    let key = ChangeResource::UpdateModelKey {
        name: "saved".into(),
        key: Secret("sk-private".into()),
    };
    assert!(!format!("{key:?}").contains("sk-private"));
    assert!(Call::new(Scope::Service, &ReadResource::Models).is_err());
    assert!(
        Call::new(
            Scope::Folder(Path::new(".")),
            &ChatResource(ReadResource::Skills)
        )
        .is_err()
    );
}
