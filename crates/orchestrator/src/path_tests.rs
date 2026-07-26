use super::*;

fn path(text: &str) -> AgentPath {
    AgentPath::parse(text).expect("valid path")
}

#[test]
fn a_child_hangs_off_its_parent() {
    let root = AgentPath::root();
    assert_eq!(root.child("survey").unwrap().as_str(), "/survey");
    assert_eq!(
        path("/survey").child("parser").unwrap().as_str(),
        "/survey/parser"
    );
}

#[test]
fn depth_counts_generations_from_the_root() {
    assert_eq!(AgentPath::root().depth(), 0);
    assert_eq!(path("/survey").depth(), 1);
    assert_eq!(path("/survey/parser").depth(), 2);
}

#[test]
fn a_relative_reference_names_a_child_not_a_sibling() {
    // Reaching a sibling relatively would let a wrong guess address a live
    // agent someone else owns.
    let here = path("/survey");
    assert_eq!(here.resolve("parser").unwrap().as_str(), "/survey/parser");
    assert_eq!(
        here.resolve("/other/parser").unwrap().as_str(),
        "/other/parser"
    );
}

#[test]
fn names_that_would_break_addressing_are_refused() {
    let root = AgentPath::root();
    assert_eq!(root.child(""), Err(PathError::Empty));
    assert!(matches!(root.child("a b"), Err(PathError::Invalid(_))));
    assert!(matches!(root.child("a/b"), Err(PathError::Invalid(_))));
    assert!(matches!(
        AgentPath::parse("survey"),
        Err(PathError::Invalid(_))
    ));
}

#[test]
fn parent_walks_back_up_and_stops_at_the_root() {
    assert_eq!(path("/survey/parser").parent().unwrap(), path("/survey"));
    assert_eq!(path("/survey").parent().unwrap(), AgentPath::root());
    assert!(matches!(
        AgentPath::root().parent(),
        Err(PathError::NoParent(_))
    ));
}

#[test]
fn under_matches_a_subtree_not_a_shared_prefix() {
    assert!(path("/survey/parser").under(&path("/survey")));
    assert!(path("/survey").under(&path("/survey")));
    assert!(path("/survey").under(&AgentPath::root()));
    // `/surveying` shares a string prefix with `/survey` and is not under it.
    assert!(!path("/surveying").under(&path("/survey")));
}

#[test]
fn name_is_the_last_segment() {
    assert_eq!(path("/survey/parser").name(), "parser");
    assert_eq!(path("/survey").name(), "survey");
    assert_eq!(AgentPath::root().name(), "/");
}

#[test]
fn a_path_round_trips_through_serde() {
    let original = path("/survey/parser");
    let text = serde_json::to_string(&original).unwrap();
    assert_eq!(text, "\"/survey/parser\"");
    assert_eq!(serde_json::from_str::<AgentPath>(&text).unwrap(), original);
    assert!(serde_json::from_str::<AgentPath>("\"nope\"").is_err());
}
