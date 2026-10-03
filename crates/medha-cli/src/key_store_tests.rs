use super::*;
use std::cell::RefCell;

/// A keychain in memory that can refuse one write or every read.
#[derive(Default)]
struct Fake {
    held: RefCell<BTreeMap<String, String>>,
    refuses: Option<&'static str>,
    unreadable: bool,
}

impl Vault for Fake {
    fn put(&self, id: &str, key: &str) -> Result<()> {
        anyhow::ensure!(self.refuses != Some(id), "denied");
        self.held.borrow_mut().insert(id.into(), key.into());
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<String>> {
        anyhow::ensure!(!self.unreadable, "denied");
        Ok(self.held.borrow().get(id).cloned())
    }

    fn forget(&self, id: &str) {
        self.held.borrow_mut().remove(id);
    }
}

fn saved() -> BTreeMap<String, String> {
    [
        ("https://a.example/v1", "key-a"),
        ("search://tavily", "key-b"),
    ]
    .map(|(id, key)| (id.to_owned(), key.to_owned()))
    .into()
}

fn ids() -> Vec<String> {
    saved().into_keys().collect()
}

#[test]
fn a_keychain_that_refuses_one_key_keeps_none_and_the_file_keeps_all() {
    let vault = Fake {
        refuses: Some("search://tavily"),
        ..Fake::default()
    };
    let mut keys = saved();
    assert!(relocate(&mut keys, &ids(), true, &vault).is_err());
    assert_eq!(keys, saved());
    assert!(vault.held.borrow().is_empty());
}

#[test]
fn a_keychain_that_cannot_be_read_is_left_holding_its_keys() {
    let vault = Fake {
        held: RefCell::new(saved()),
        unreadable: true,
        ..Fake::default()
    };
    let mut keys = BTreeMap::new();
    assert!(relocate(&mut keys, &ids(), false, &vault).is_err());
    assert!(keys.is_empty());
    assert_eq!(*vault.held.borrow(), saved());
}

#[test]
fn keys_move_there_and_back_without_a_copy_left_behind() {
    let vault = Fake::default();
    let mut keys = saved();
    assert!(
        relocate(&mut keys, &ids(), true, &vault)
            .unwrap()
            .is_empty()
    );
    assert!(keys.is_empty());
    assert_eq!(*vault.held.borrow(), saved());

    let stale = relocate(&mut keys, &ids(), false, &vault).unwrap();
    assert_eq!(keys, saved());
    assert_eq!(stale, ids());
}

#[test]
fn a_move_out_of_the_keychain_looks_for_every_kind_of_saved_key() {
    let cfg: Config = toml::from_str(
        r#"
[models.cloud]
protocol = "open-ai-chat"
base_url = "https://a.example/v1"
model = "big"
auth = "bearer"

[mcp.notes]
url = "https://notes.example/mcp"
auth = "oauth"
"#,
    )
    .unwrap();
    let server = &cfg.mcp["notes"];
    let ids = credential_ids(&cfg);
    for id in [
        "https://a.example/v1".to_owned(),
        "search://tavily".to_owned(),
        "search://brave".to_owned(),
        mcp_key_id("notes", server),
        mcp_oauth_id("notes", &server.url),
    ] {
        assert!(ids.contains(&id), "{id} would be stranded in the keychain");
    }
}
