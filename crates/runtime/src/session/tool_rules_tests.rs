use super::*;
use protocol::OffReason;

#[test]
fn every_setting_that_keeps_a_tool_out_is_named_and_none_can_add_one() {
    let project = lockfile::MedhaLock::parse(
        "[tools]\npreset = \"minimal\"\nallow = [\"read\", \"web\"]\ndeny = [\"read\"]\n",
    )
    .unwrap()
    .tools;
    let user = kernel::tool_pattern::ToolFilter {
        allow: vec![],
        deny: vec!["edit".into()],
    };
    assert_eq!(
        off_reasons("read", &project, &user),
        [OffReason::ProjectDeny]
    );
    assert_eq!(
        off_reasons("web", &project, &user),
        [OffReason::Core],
        "an allow list cannot add past the preset"
    );
    assert_eq!(
        off_reasons("edit", &project, &user),
        [OffReason::ProjectAllow, OffReason::UserDeny]
    );
    let open = lockfile::ToolsConfig::default();
    assert!(off_reasons("web", &open, &Default::default()).is_empty());
}
