//! The size of the tool catalogue, measured rather than assumed.
//!
//! Every tool's name, description and schema is re-sent on every request of
//! every turn, so the catalogue is a fixed tax on the whole session. These
//! bounds are deliberately close to the current figures: crossing one is not a
//! failure, it is a prompt to look at what was added and decide it earns its
//! place.

use kernel::Executor;
use std::sync::Arc;
use tools::ToolRegistry;

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

/// Delegation has to be registered for its tools to exist, but nothing here
/// runs a child.
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

/// Registered names, descriptions and schemas, exactly as a provider is given
/// them for the default static catalogue. Includes MCP controls, but not tools
/// supplied by connected servers or optional writer-worktree agent tools.
fn catalogue() -> Vec<(String, usize)> {
    let dir = std::env::temp_dir().join(format!("medha-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sandbox = Arc::new(sandbox::WorkspaceSandbox::new_jailed(&dir).unwrap());
    let mut registry = ToolRegistry::with_workspace(sandbox, Arc::new(NoArtifacts));
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
    registry.register_mcp(Arc::new(mcp::McpManager::new(
        dir.clone(),
        mcp::Config::default(),
    )));
    registry.register_session_search(
        Arc::new(store::SqliteLog::open(dir.join("events.db")).unwrap()),
        Arc::new(NoArtifacts),
    );
    registry.register_agents(
        Arc::new(orchestrator::AgentControl::new(
            Arc::new(NoChildren),
            tokio_util::sync::CancellationToken::new(),
        )),
        8,
    );
    registry.register_skills(Arc::new(tools::SkillStore::new(dir.join("project"), None)));
    let specs = registry
        .specs()
        .into_iter()
        .map(|spec| {
            let size = spec.name.len() + spec.description.len() + spec.schema.to_string().len();
            (spec.name, size)
        })
        .collect();
    std::fs::remove_dir_all(&dir).ok();
    specs
}

#[test]
fn the_tool_catalogue_stays_within_its_budget() {
    let catalogue = catalogue();
    let names: Vec<&str> = catalogue.iter().map(|(name, _)| name.as_str()).collect();
    let chars: usize = catalogue.iter().map(|(_, size)| size).sum();

    assert!(
        catalogue.len() <= 24,
        "{} static tools registered: {names:?}",
        catalogue.len()
    );
    assert!(
        chars <= 32_000,
        "the static tool components cost {chars} UTF-8 bytes: {names:?}. \
         The CLI request-budget test covers system text and provider serialization."
    );
}

/// A tool nobody can name is a tool nobody calls. Names are the vocabulary the
/// model is given, so they follow one rule: lowercase, and dotted only where a
/// namespace really has several entry points.
#[test]
fn every_tool_name_follows_one_convention() {
    for (name, _) in catalogue() {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
            "'{name}' is not a lowercase tool name"
        );
        assert!(
            name.matches('.').count() <= 1,
            "'{name}' nests deeper than namespace.verb"
        );
    }
}

/// The minimal preset has to stay callable on its own: a session narrowed to it
/// can still read, change, search and run things, and everything the full set
/// offers is reachable through the shell at the cost of more model work.
#[test]
fn the_minimal_preset_names_tools_that_exist() {
    let registered: std::collections::HashSet<String> =
        catalogue().into_iter().map(|(name, _)| name).collect();
    for name in lockfile::MINIMAL_TOOLS {
        assert!(
            registered.contains(name),
            "the minimal preset names '{name}', which is not registered"
        );
    }
}
