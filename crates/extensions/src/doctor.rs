//! `medha plugins doctor`: finds what would stop an enabled plugin from working
//! and says how to fix it.

use crate::store::{Activation, Store};
use crate::{ExtensionComponent, PLUGIN_ROOT_PLACEHOLDER};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    pub subject: String,
    pub message: String,
}

impl Check {
    fn new(level: Level, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            subject: subject.into(),
            message: message.into(),
        }
    }
}

pub fn run(store: &Store) -> Vec<Check> {
    let mut checks = Vec::new();
    let sandbox = sandbox::select_backend(
        &sandbox::SandboxConfig::default(),
        Vec::new(),
        sandbox::ApprovedRoots::default(),
        sandbox::NetworkGrant::default(),
    );
    checks.push(
        if sandbox.containment() == kernel::Containment::OsFsJailNoNet {
            Check::new(
                Level::Ok,
                "sandbox",
                "hooks run in an OS sandbox without network",
            )
        } else {
            Check::new(
                Level::Fail,
                "sandbox",
                "no OS sandbox is available, so plugin hooks will not run; on Linux, use a kernel \
             with Landlock (5.13+)",
            )
        },
    );
    checks.push(if find_program("git").is_some() {
        Check::new(Level::Ok, "git", "installing from repositories works")
    } else {
        Check::new(
            Level::Warn,
            "git",
            "git is not on PATH, so only local folders can be installed; install git",
        )
    });
    let discovery = match store.discover() {
        Ok(discovery) => discovery,
        Err(error) => {
            checks.push(Check::new(Level::Fail, "plugins", error.to_string()));
            return checks;
        }
    };
    for notice in discovery.notices() {
        checks.push(Check::new(Level::Warn, "plugins", notice));
    }
    for plugin in &discovery.plugins {
        if plugin.activation != Activation::Enabled {
            continue;
        }
        let id = &plugin.package.manifest.id;
        let before = checks.len();
        for component in &plugin.package.manifest.components {
            check_component(&plugin.package.root, id, component, &mut checks);
        }
        if let Some(note) = plugin
            .package
            .manifest
            .description
            .as_deref()
            .and_then(|text| text.split_once("not used by Medha: "))
        {
            checks.push(Check::new(
                Level::Warn,
                id.clone(),
                format!("skipped parts: {}", note.1.trim_end_matches(')')),
            ));
        }
        let data = store.data_dir(id);
        if std::fs::create_dir_all(&data).is_err() {
            checks.push(Check::new(
                Level::Fail,
                id.clone(),
                format!("its data folder {} cannot be created", data.display()),
            ));
        }
        if checks.len() == before {
            checks.push(Check::new(Level::Ok, id.clone(), "ready"));
        }
    }
    checks
}

fn check_component(root: &Path, id: &str, component: &ExtensionComponent, out: &mut Vec<Check>) {
    let subject = format!("{id}/{}", component.id());
    match component {
        ExtensionComponent::Hook { entrypoint, .. } if entrypoint.shell => {
            let program = first_word(&entrypoint.program);
            if !program.contains(['$', '/']) && find_program(&program).is_none() {
                out.push(Check::new(
                    Level::Fail,
                    subject,
                    format!("it runs `{program}`, which is not on PATH; install it"),
                ));
            }
        }
        ExtensionComponent::Hook { entrypoint, .. } => {
            if !root.join(&entrypoint.program).is_file() {
                out.push(Check::new(
                    Level::Fail,
                    subject,
                    format!("{} is missing", entrypoint.program),
                ));
            }
        }
        ExtensionComponent::Mcp {
            command, url: None, ..
        } => {
            let Some(program) = command.first() else {
                return;
            };
            let found = match program.strip_prefix(PLUGIN_ROOT_PLACEHOLDER) {
                Some(rest) => root.join(rest.trim_start_matches('/')).is_file(),
                None => find_program(program).is_some(),
            };
            if !found {
                out.push(Check::new(
                    Level::Fail,
                    subject,
                    format!("MCP server command `{program}` was not found; install it"),
                ));
            }
        }
        _ => {}
    }
}

fn first_word(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        cfg!(windows)
            .then(|| dir.join(format!("{name}.exe")))
            .filter(|candidate| candidate.is_file())
    })
}
