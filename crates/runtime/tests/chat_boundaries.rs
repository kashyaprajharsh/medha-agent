//! Many chats in one process: what one chat is granted, another does not get.

mod common;

use common::{chat, chat_with, endpoint, open, say};
use kernel::{Approval, ExecutionAccess, Message, NetworkDecision, Role, StopReason};
use runtime::session::Started;
use std::path::Path;
use std::sync::Arc;

/// Says yes for the rest of the chat, which is the grant that must not travel.
struct ForThisChat;

#[async_trait::async_trait]
impl kernel::HumanGate for ForThisChat {
    async fn confirm(&self, _: &str, _: Option<&str>, _: bool) -> Approval {
        Approval::Once
    }
    async fn confirm_network(&self, _: Option<&str>, _: bool) -> NetworkDecision {
        NetworkDecision::Session
    }
    async fn confirm_access(&self, _: Option<&str>, _: bool) -> NetworkDecision {
        NetworkDecision::Session
    }
}

/// What a jailed `cat` printed, or nothing when the jail refused it.
async fn cat(chat: &Started, path: &Path) -> String {
    let args = [path.display().to_string()];
    match chat
        .workspace
        .exec("/bin/cat", &args, Vec::new(), false)
        .await
    {
        Ok(out) if out.status == Some(0) => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_given_to_one_chat_is_not_usable_from_another() {
    let root = tempfile::tempdir().unwrap();
    let (address, _) = endpoint().await;
    // SAFETY: this file holds one test, so nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("MEDHA_HOME", root.path().join("home")) };
    let (folder_a, folder_b) = (open(&root.path().join("a")), open(&root.path().join("b")));
    let jailed = sandbox::native_backend_available();

    let granted = chat_with(&folder_a, &address, "m", Arc::new(ForThisChat)).await;
    let sibling = chat(&folder_a, &address, "m").await;
    let stranger = chat(&folder_b, &address, "m").await;
    let others = [&sibling, &stranger];
    let permissions = |chat: &Started| chat.workspace.permission_manager();

    assert_eq!(
        granted
            .workspace
            .request_network(Some("fetch"), false)
            .await,
        NetworkDecision::Session
    );
    assert!(permissions(&granted).network_grant().granted());
    for other in others {
        assert!(!permissions(other).network_grant().granted());
    }

    let outside = root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = outside.canonicalize().unwrap();
    let note = outside.join("note");
    std::fs::write(&note, "outside secret").unwrap();
    let access = ExecutionAccess {
        read_paths: vec![outside.clone()],
        ..Default::default()
    };
    let decision = permissions(&granted)
        .request_execution_access(&access, "read outside", false)
        .await
        .unwrap();
    assert_eq!(decision, NetworkDecision::Session);
    assert!(
        permissions(&granted)
            .approved_roots()
            .read_roots()
            .contains(&outside)
    );
    for other in others {
        assert!(
            !permissions(other)
                .approved_roots()
                .read_roots()
                .contains(&outside)
        );
    }
    if jailed {
        assert_eq!(cat(&granted, &note).await, "outside secret");
        for other in others {
            assert_eq!(cat(other, &note).await, "", "another chat used the grant");
        }
    }

    let inside = folder_a.root.join("inside.txt");
    std::fs::write(&inside, "folder a").unwrap();
    let inside_path = inside.display().to_string();
    assert_eq!(
        sibling.workspace.read("inside.txt").await.unwrap(),
        "folder a"
    );
    assert!(stranger.workspace.read(&inside_path).await.is_err());
    assert!(
        stranger
            .workspace
            .write(&inside_path, "taken")
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "folder a");
    if jailed {
        assert_eq!(
            cat(&stranger, &inside).await,
            "",
            "a chat read another folder"
        );
    }

    let (mine, theirs) = (
        granted.scratch.as_ref().unwrap(),
        sibling.scratch.as_ref().unwrap(),
    );
    assert_ne!(mine.path(), theirs.path());
    let draft = mine.path().join("draft");
    std::fs::write(&draft, "scratch secret").unwrap();
    if jailed {
        assert_eq!(cat(&granted, &draft).await, "scratch secret");
        assert_eq!(
            cat(&sibling, &draft).await,
            "",
            "a chat read another's scratch"
        );
    }

    // What admits a client to the backend is outside every chat's reach unless its person opens it.
    let token = root.path().join("home").join("serve").join("token");
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(&token, "backend token").unwrap();
    assert!(
        sibling
            .workspace
            .read(&token.display().to_string())
            .await
            .is_err()
    );
    if jailed {
        assert_eq!(cat(&sibling, &token).await, "", "a command read the token");
    }

    assert!(!Arc::ptr_eq(&granted.agent_budget, &sibling.agent_budget));
    let spent = kernel::Budget {
        max_tokens: Some(1),
        ..granted.base_budget.clone()
    };
    let asked = vec![
        Message::system(granted.system.clone()),
        Message::new(Role::User, "over budget"),
    ];
    let (_, stopped) = granted
        .kernel
        .run_session(
            &granted.session,
            asked,
            spent.with_fresh_pool(),
            &kernel::NullSink,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(stopped, StopReason::Budget(_)), "{stopped:?}");
    assert_eq!(say(&sibling, "still here").await, "echo: still here");
}
