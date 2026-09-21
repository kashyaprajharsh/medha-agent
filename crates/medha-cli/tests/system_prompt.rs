//! The operating brief and the tool registry have to agree.
//!
//! The brief tells the model what it can call. A name in it that no tool
//! answers to sends the model to collect an "unknown tool" error and burn a
//! turn, and nothing else in the build notices — the brief is a string, the
//! registry is code, and they drift apart silently.

use std::collections::HashSet;
use std::sync::Arc;

struct NoArtifacts;
impl kernel::ArtifactStore for NoArtifacts {
    fn put(&self, _bytes: &[u8]) -> Result<String, String> {
        Ok("hash".into())
    }
    fn get(&self, _hash: &str, _offset: usize, _len: Option<usize>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn size(&self, _hash: &str) -> Result<usize, String> {
        Ok(0)
    }
}

/// Everything a `` `backticked` `` identifier in the brief is allowed to be:
/// a tool's name, one of its arguments, or one of the values an argument
/// accepts. Collected from the registry itself, so it moves when the tools do.
fn vocabulary() -> HashSet<String> {
    let dir = std::env::temp_dir().join(format!("medha-brief-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = Arc::new(sandbox::WorkspaceSandbox::new_jailed(&dir).unwrap());
    let mut registry = tools::ToolRegistry::with_workspace(workspace, Arc::new(NoArtifacts));
    registry.register_lsp(Arc::new(lsp::LspManager::new(
        dir.clone(),
        lsp::Config {
            enabled: false,
            ..lsp::Config::default()
        },
    )));
    registry.register_memory(Arc::new(
        memory::MemoryProjection::open(dir.join("project.db"), dir.join("user.db")).unwrap(),
    ));
    registry.register_skills(Arc::new(tools::SkillStore::new(dir.join("project"), None)));

    registry.register_agents(
        Arc::new(orchestrator::AgentControl::new(
            Arc::new(NoChildren),
            tokio_util::sync::CancellationToken::new(),
        )),
        8,
    );

    let mut known = HashSet::new();
    for spec in kernel::Executor::specs(&registry) {
        known.insert(spec.name);
        collect(&spec.schema, &mut known);
    }
    std::fs::remove_dir_all(&dir).ok();
    known
}

/// Delegation has to be registered for its schemas to be read, but nothing in
/// this test runs a child.
struct NoChildren;
#[async_trait::async_trait]
impl orchestrator::ChildRunner for NoChildren {
    async fn run(
        &self,
        _run: orchestrator::ChildRun,
    ) -> Result<orchestrator::ChildOutcome, String> {
        Err("no children here".into())
    }
}

/// Every property name and every enumerated value anywhere in a schema.
fn collect(schema: &serde_json::Value, into: &mut HashSet<String>) {
    match schema {
        serde_json::Value::Object(fields) => {
            if let Some(serde_json::Value::Object(properties)) = fields.get("properties") {
                into.extend(properties.keys().cloned());
            }
            if let Some(serde_json::Value::Array(values)) = fields.get("enum") {
                into.extend(values.iter().filter_map(|v| v.as_str()).map(String::from));
            }
            for value in fields.values() {
                collect(value, into);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, into)),
        _ => {}
    }
}

#[test]
fn the_brief_names_only_tools_and_arguments_that_exist() {
    let known = vocabulary();
    // Words the brief quotes as literals rather than as anything callable.
    let prose: HashSet<&str> = ["true", "false"].into_iter().collect();
    let brief = context::identity::system_prompt(None);

    // The brief has no code fences, so every odd segment between backticks is
    // one quoted span. Only the spans shaped like an identifier are checked;
    // the rest quote paths, values and phrases.
    let unknown: Vec<&str> = brief
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| {
            span.starts_with(|c: char| c.is_ascii_lowercase())
                && span
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
        })
        .filter(|name| !prose.contains(name) && !known.contains(*name))
        .collect();
    assert!(
        unknown.is_empty(),
        "the operating brief names {unknown:?}, which is neither a registered tool, \
         one of its arguments, nor a value one of them accepts"
    );
}

/// The brief is re-sent whole on every request of every turn, so its length is
/// a per-turn tax. This bound is close to its current size on purpose.
#[test]
fn the_brief_stays_within_its_budget() {
    let brief = context::identity::system_prompt(None);
    assert!(
        brief.len() <= 7_500,
        "the operating brief is {} characters (~{} tokens) on every request",
        brief.len(),
        brief.len() / 4
    );
}
