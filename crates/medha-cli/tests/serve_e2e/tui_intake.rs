//! The TUI's way in: what a source is before it is added, the For choice, and drafts written in the person's own editor.
use super::*;
use protocol::{ChangeResource, DraftKind, ReadResource, ResourceResult};

async fn read(client: &mut Client, folder: &Path, read: ReadResource) -> ResourceResult {
    let reply = client
        .about(
            folder,
            json!({"method": "application.resources", "params": read}),
        )
        .await
        .unwrap();
    serde_json::from_value(reply).unwrap()
}

async fn change(
    client: &mut Client,
    folder: &Path,
    change: ChangeResource,
) -> Result<ResourceResult, String> {
    client
        .about(
            folder,
            json!({"method": "application.resources.change", "params": change}),
        )
        .await
        .map(|reply| serde_json::from_value(reply).unwrap())
}

/// A draft is a template in the person's drafts; once edited it previews and adds like any source, here for this project.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_draft_is_written_previewed_and_added_for_this_project() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("tui-intake");
    client.open(&folder).await;

    let made = change(
        &mut client,
        &folder,
        ChangeResource::NewDraft {
            kind: DraftKind::Skill,
            name: "notes".into(),
            text: None,
        },
    )
    .await
    .unwrap();
    let draft = world.home().join("drafts/notes");
    assert!(
        matches!(&made, ResourceResult::Changed(notice) if notice.messages.join(" ").contains(&draft.display().to_string())),
        "{made:?}"
    );
    let skill_md = draft.join("SKILL.md");
    let text = std::fs::read_to_string(&skill_md).unwrap();
    std::fs::write(
        &skill_md,
        text.replace(
            "What it does and when to use it.",
            "Keeps meeting notes in one plain format.",
        ),
    )
    .unwrap();

    let seen = read(
        &mut client,
        &folder,
        ReadResource::Preview {
            source: draft.display().to_string(),
        },
    )
    .await;
    let ResourceResult::Preview(preview) = seen else {
        panic!("{seen:?}")
    };
    assert_eq!(preview.kind, "skill");
    assert_eq!(preview.name.as_deref(), Some("notes"));
    assert_eq!(
        preview.description.as_deref(),
        Some("Keeps meeting notes in one plain format.")
    );
    assert!(
        preview.files.contains(&"SKILL.md".to_string()),
        "{preview:?}"
    );

    change(
        &mut client,
        &folder,
        ChangeResource::InstallSkill {
            source: draft.display().to_string(),
            project: true,
        },
    )
    .await
    .unwrap();
    assert!(folder.join(".medha/skills/notes/SKILL.md").is_file());
    assert!(!world.home().join("skills/notes").exists());

    let again = change(
        &mut client,
        &folder,
        ChangeResource::NewDraft {
            kind: DraftKind::Skill,
            name: "notes".into(),
            text: None,
        },
    )
    .await;
    assert!(
        again.is_err_and(|why| why.contains("already")),
        "a draft was overwritten"
    );
    let bad = change(
        &mut client,
        &folder,
        ChangeResource::NewDraft {
            kind: DraftKind::Skill,
            name: "../escape".into(),
            text: None,
        },
    )
    .await;
    assert!(bad.is_err());

    change(
        &mut client,
        &folder,
        ChangeResource::NewDraft {
            kind: DraftKind::Plugin,
            name: "dev.me.tools".into(),
            text: None,
        },
    )
    .await
    .unwrap();
    let proposed = "---\nname: proposed\ndescription: Was proposed in a chat.\n---\n\n1. Ask.\n";
    change(
        &mut client,
        &folder,
        ChangeResource::NewDraft {
            kind: DraftKind::Skill,
            name: "proposed".into(),
            text: Some(proposed.into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(world.home().join("drafts/proposed/SKILL.md")).unwrap(),
        proposed
    );

    let seen = read(
        &mut client,
        &folder,
        ReadResource::Preview {
            source: world
                .home()
                .join("drafts/dev.me.tools")
                .display()
                .to_string(),
        },
    )
    .await;
    let ResourceResult::Preview(preview) = seen else {
        panic!("{seen:?}")
    };
    assert_eq!(preview.kind, "plugin", "{preview:?}");
    assert!(preview.refused.is_none(), "{preview:?}");
}
