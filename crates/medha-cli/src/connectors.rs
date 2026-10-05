//! The reviewed list of vendor-hosted MCP servers, shared by the desktop and the
//! TUI. Connecting one creates an ordinary server entry.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use kernel::{Event, EventKind, EventLog};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::McpServer;

macro_rules! entries {
    ($($id:literal),* $(,)?) => {
        &[$(($id, include_str!(concat!("../connectors/", $id, ".toml")))),*]
    };
}

const FILES: &[(&str, &str)] = entries![
    "adobe",
    "airtable",
    "algolia",
    "alphavantage",
    "amplitude",
    "apify",
    "astro-docs",
    "atlassian",
    "attio",
    "aws-knowledge",
    "betterstack",
    "buildkite",
    "calendly",
    "canva",
    "clickup",
    "close",
    "cloudflare",
    "cloudinary",
    "coda",
    "comfy-cloud",
    "contentful",
    "context7",
    "coros",
    "craft",
    "datadog",
    "deepwiki",
    "drawio",
    "dropbox",
    "egnyte",
    "exa",
    "excalidraw",
    "fathom",
    "figma",
    "fireflies",
    "gamma",
    "gitlab",
    "gitmcp",
    "globalping",
    "grafana",
    "granola",
    "guru",
    "hex",
    "honeycomb",
    "hugging-face",
    "intercom",
    "jam",
    "javadocs",
    "klaviyo",
    "langchain-docs",
    "linear",
    "lucid",
    "mermaid-chart",
    "microsoft-learn",
    "miro",
    "mixpanel",
    "monday",
    "motherduck",
    "neon",
    "netlify",
    "notion",
    "otter",
    "parallel",
    "plane",
    "planetscale",
    "posthog",
    "postman",
    "prisma-postgres",
    "sanity",
    "semgrep",
    "sentry",
    "shortcut",
    "supabase",
    "svelte",
    "tavily",
    "todoist",
    "twelve-data",
    "typeform",
    "webflow",
    "wolfram",
    "wordpress-com",
    "zapier",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Category {
    Productivity,
    Communication,
    Design,
    Developer,
    Analytics,
    Finance,
    Sales,
    Health,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SignIn {
    Oauth,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connector {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub description: String,
    pub url: String,
    pub sign_in: SignIn,
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default)]
    pub asks: Vec<String>,
    /// Signs a project already uses this vendor: a file, `npm:<pkg>` or `pypi:<pkg>`.
    #[serde(default, skip_serializing)]
    pub detect: Vec<String>,
}

static CATALOG: LazyLock<Vec<Connector>> = LazyLock::new(|| {
    let mut all: Vec<Connector> = FILES
        .iter()
        .filter_map(|(_, text)| toml::from_str(text).ok())
        .collect();
    all.sort_by(|a, b| (a.category, &a.name).cmp(&(b.category, &b.name)));
    all
});

pub(crate) fn catalog() -> &'static [Connector] {
    &CATALOG
}

pub(crate) fn find(id: &str) -> Option<&'static Connector> {
    catalog().iter().find(|connector| connector.id == id)
}

impl Connector {
    /// Trusted only reconnects it at launch; every call still asks first.
    pub(crate) fn server(&self) -> McpServer {
        McpServer {
            url: self.url.clone(),
            auth: match self.sign_in {
                SignIn::Oauth => "oauth",
                SignIn::None => "none",
            }
            .into(),
            trust: "trusted".into(),
            allow_tools: self.tools.clone(),
            ..Default::default()
        }
    }

    pub(crate) fn configured<'a>(
        &self,
        servers: &'a BTreeMap<String, McpServer>,
    ) -> Option<&'a str> {
        servers
            .iter()
            .find(|(_, server)| same_address(&server.url, &self.url))
            .map(|(id, _)| id.as_str())
    }

    pub(crate) fn install(&self, servers: &mut BTreeMap<String, McpServer>) -> String {
        if let Some(id) = self.configured(servers) {
            return id.to_owned();
        }
        let id = std::iter::once(self.id.clone())
            .chain((2..).map(|n| format!("{}-{n}", self.id)))
            .find(|id| !servers.contains_key(id))
            .expect("an unused id");
        servers.insert(id.clone(), self.server());
        id
    }
}

/// What a workspace already depends on, read once for every connector.
pub(crate) struct Project {
    root: PathBuf,
    npm: BTreeSet<String>,
    python: String,
}

impl Project {
    pub(crate) fn scan(root: &Path) -> Self {
        let npm = std::fs::read_to_string(root.join("package.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|manifest| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .filter_map(|key| manifest[key].as_object())
                    .flat_map(|deps| deps.keys().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let python = ["requirements.txt", "pyproject.toml"]
            .iter()
            .filter_map(|file| std::fs::read_to_string(root.join(file)).ok())
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        Self {
            root: root.to_owned(),
            npm,
            python,
        }
    }

    pub(crate) fn evidence(&self, connector: &Connector) -> Option<String> {
        connector
            .detect
            .iter()
            .find_map(|rule| match rule.split_once(':') {
                Some(("npm", name)) => self
                    .npm
                    .contains(name)
                    .then(|| format!("Found {name} in package.json")),
                Some(("pypi", name)) => self
                    .python
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .any(|word| word == name)
                    .then(|| format!("Found {name} in your Python dependencies")),
                _ => self
                    .root
                    .join(rule)
                    .exists()
                    .then(|| format!("Found {rule}")),
            })
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Activity {
    pub last_used: f64,
    pub this_week: u32,
}

/// Calls that ran, per server. The kernel writes an effect marker only once a
/// call is authorized, and every MCP call needs the user's approval.
pub(crate) fn activity(events: &[Event], now: f64, tally: &mut HashMap<String, Activity>) {
    for event in events
        .iter()
        .filter(|event| event.kind == EventKind::ToolEffectPrepared)
    {
        let Some((server, _)) = event.payload["tool"]
            .as_str()
            .and_then(|tool| tool.strip_prefix(mcp::TOOL_PREFIX))
            .and_then(|rest| rest.split_once("__"))
        else {
            continue;
        };
        let seen = tally.entry(server.to_owned()).or_default();
        seen.last_used = seen.last_used.max(event.ts);
        if now - event.ts <= 7.0 * 86_400.0 {
            seen.this_week += 1;
        }
    }
}

pub(crate) async fn listing<L: EventLog>(
    log: &L,
    workspace: &Path,
    servers: &BTreeMap<String, McpServer>,
    now: f64,
) -> Value {
    let mut used = HashMap::new();
    let recent: std::collections::HashSet<_> = log
        .sessions()
        .await
        .into_iter()
        .filter(|session| now - session.last_ts <= 30.0 * 86_400.0)
        .map(|session| session.id)
        .collect();
    // Only the calls are read: loading each chat whole took longer the more Medha was used.
    let mut calls = log.events_of_kind(EventKind::ToolEffectPrepared).await;
    calls.retain(|call| recent.contains(&call.session_id));
    activity(&calls, now, &mut used);
    let project = Project::scan(workspace);
    let rows: Vec<Value> = catalog()
        .iter()
        .map(|connector| {
            let server = connector.configured(servers);
            let off = server.is_some_and(|id| servers[id].disabled);
            let mut row = serde_json::to_value(connector).unwrap_or_default();
            row["server"] = json!(server);
            row["off"] = json!(off);
            row["activity"] = json!(server.and_then(|id| used.get(id)));
            row["evidence"] = json!(
                (server.is_none() || off)
                    .then(|| project.evidence(connector))
                    .flatten()
            );
            row
        })
        .collect();
    json!({ "connectors": rows })
}

fn same_address(a: &str, b: &str) -> bool {
    let key = |raw: &str| {
        url::Url::parse(raw).ok().map(|url| {
            (
                url.scheme().to_owned(),
                url.host_str().map(str::to_owned),
                url.port_or_known_default(),
                url.path().trim_end_matches('/').to_owned(),
                url.query().map(str::to_owned),
            )
        })
    };
    key(a).is_some_and(|a| Some(a) == key(b))
}

#[cfg(test)]
#[path = "connectors_tests.rs"]
mod tests;
