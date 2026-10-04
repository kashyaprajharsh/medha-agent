//! Which tools ask before they run.

/// `raw` is the caller's override, as `MEDHA_APPROVE` spells it; empty means none.
pub fn approve_list_from(base: Vec<String>, raw: &str) -> Vec<String> {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parts.contains(&"none") {
        return Vec::new();
    }

    // Delegation is gated for irreversible token spend, not filesystem radius.
    // One name covers both starting a child and giving one more work.
    let mut out = base;
    out.extend(["shell.exec", "agent.spawn"].map(String::from));
    for part in parts {
        match part {
            "all" => out.extend(["write", "edit", "shell.exec"].map(String::from)),
            "writes" => out.extend(["write", "edit"].map(String::from)),
            "shell" => out.push("shell.exec".into()),
            "agents" => out.push("agent.spawn".into()),
            other => out.push(other.to_string()),
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Names in `[policy].approve` that match no registered tool, in the order the
/// lock file lists them.
///
/// The approve list is matched against a tool's name, so a name that is not a
/// tool gates nothing — and says so nowhere. A tool that was renamed or folded
/// into another leaves exactly that behind: `approve = ["fs.write"]` reads as a
/// configured gate and behaves as no gate at all. Report it rather than resolve
/// it, so the lock file gets corrected once instead of translated forever.
pub fn unknown_approvals(
    approve: &[String],
    registered: &std::collections::HashSet<String>,
) -> Vec<String> {
    approve
        .iter()
        .filter(|name| !registered.contains(*name))
        .cloned()
        .collect()
}
