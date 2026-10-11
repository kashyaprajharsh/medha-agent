use super::*;
use crate::{BlastRadius, ToolCategory};

fn tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        schema: serde_json::json!({"type":"object"}),
        blast_radius: BlastRadius::Read,
        category: ToolCategory::Other,
        icon: String::new(),
    }
}

#[test]
fn appended_tools_do_not_change_prior_definitions_even_after_restart() {
    let first = ToolSnapshot::merge(
        None,
        ToolInventory {
            specs: vec![tool("z", "old")],
            ..Default::default()
        },
        false,
    );
    let restored: ToolSnapshot = serde_json::from_value(first.value()).unwrap();
    let next = ToolSnapshot::merge(
        Some(&restored),
        ToolInventory {
            specs: vec![tool("a", "new"), tool("z", "changed")],
            ..Default::default()
        },
        false,
    );
    assert_eq!(
        next.specs
            .iter()
            .map(|spec| (spec.name.as_str(), spec.description.as_str()))
            .collect::<Vec<_>>(),
        [("z", "old"), ("a", "new")]
    );
    let refreshed = ToolSnapshot::merge(
        Some(&next),
        ToolInventory {
            specs: vec![tool("a", "new"), tool("z", "changed")],
            ..Default::default()
        },
        true,
    );
    assert_eq!(refreshed.specs[0].name, "z");
    assert_eq!(refreshed.specs[0].description, "changed");
}

#[test]
fn unavailable_tools_survive_only_with_the_same_registration() {
    let sources = BTreeMap::from([("remote".into(), "server-one".into())]);
    let first = ToolSnapshot::merge(
        None,
        ToolInventory {
            specs: vec![tool("remote", "old")],
            sources: sources.clone(),
            ..Default::default()
        },
        false,
    );
    let unavailable = ToolSnapshot::merge(
        Some(&first),
        ToolInventory {
            sources,
            unavailable: BTreeSet::from(["remote".into()]),
            ..Default::default()
        },
        false,
    );
    assert_eq!(unavailable.specs.len(), 1);
    let removed = ToolSnapshot::merge(Some(&unavailable), ToolInventory::default(), false);
    assert!(removed.specs.is_empty());
    let replaced = ToolSnapshot::merge(
        Some(&first),
        ToolInventory {
            specs: vec![tool("remote", "replacement")],
            sources: BTreeMap::from([("remote".into(), "server-two".into())]),
            ..Default::default()
        },
        false,
    );
    assert_eq!(replaced.specs[0].description, "replacement");
}

#[test]
fn an_upgrade_cannot_relabel_an_unavailable_old_schema_as_current() {
    let mut old = ToolSnapshot::merge(
        None,
        ToolInventory {
            specs: vec![tool("local", "old local"), tool("remote", "old remote")],
            sources: BTreeMap::from([("remote".into(), "server".into())]),
            ..Default::default()
        },
        false,
    );
    old.version = "earlier release".into();
    let restarted = ToolSnapshot::merge(
        Some(&old),
        ToolInventory {
            specs: vec![tool("local", "new local")],
            sources: old.sources.clone(),
            unavailable: BTreeSet::from(["remote".into()]),
        },
        false,
    );
    assert_eq!(restarted.specs.len(), 1);
    assert_eq!(restarted.specs[0].description, "new local");
    let connected = ToolSnapshot::merge(
        Some(&restarted),
        ToolInventory {
            specs: vec![tool("remote", "new remote"), tool("local", "new local")],
            sources: old.sources.clone(),
            ..Default::default()
        },
        false,
    );
    assert_eq!(connected.specs[0].name, "local");
    assert_eq!(connected.specs[1].description, "new remote");
}
