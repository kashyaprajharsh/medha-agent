//! Execution of operator action templates owned by enabled plugins.

use crate::{Activation, Error, ListedPlugin, OperatorAction, Store};
use std::path::Path;
use std::time::Duration;

const ARGUMENTS: &str = "$ARGUMENTS";
const MAX_PROMPT: usize = 65_536;

impl Store {
    /// Expand a pinned action's plugin-authored snippets under its approved
    /// filesystem grant. Caller arguments are inserted only after parsing the
    /// template, so they cannot introduce a command to execute.
    pub async fn expand_action_snippets(
        &self,
        action_id: &str,
        expected_prompt: &str,
        args: &str,
        workspace: &Path,
    ) -> Result<String, Error> {
        let action = self
            .actions()?
            .into_iter()
            .find(|action| action.id == action_id && action.prompt == expected_prompt)
            .ok_or_else(|| {
                Error::Action("plugin changed or was disabled before its command ran".into())
            })?;
        let plugin = self.inspect(&action.plugin_id, None)?;
        if plugin.activation != Activation::Enabled {
            return Err(Error::Action(
                "plugin changed or was disabled before its command ran".into(),
            ));
        }
        let component = action
            .id
            .rsplit_once('/')
            .map_or(action.id.as_str(), |(_, id)| id);
        let hash = plugin.package.content_hash.clone();
        self.health.begin(&action.plugin_id, component, &hash);
        let result = expand_enabled(self, plugin, &action, expected_prompt, args, workspace).await;
        self.health.finish(
            &action.plugin_id,
            component,
            &hash,
            result.as_ref().err().map(|_| "plugin action failed"),
        );
        result
    }
}

async fn expand_enabled(
    store: &Store,
    plugin: ListedPlugin,
    action: &OperatorAction,
    expected_prompt: &str,
    args: &str,
    workspace: &Path,
) -> Result<String, Error> {
    let grant = plugin.grant.unwrap_or_default();
    let root = plugin.package.root;
    let data = store.data_dir(&action.plugin_id);
    std::fs::create_dir_all(&data).map_err(|error| Error::Action(error.to_string()))?;
    let backend = sandbox::select_backend(
        &sandbox::SandboxConfig::default(),
        Vec::new(),
        sandbox::ApprovedRoots::default(),
        sandbox::NetworkGrant::default(),
    );
    let mut output = String::new();
    let mut rest = expected_prompt;
    let mut count = 0;
    while let Some(start) = rest.find("!`") {
        output.push_str(&rest[..start].replace(ARGUMENTS, args));
        let after = &rest[start + 2..];
        let end = after
            .find('`')
            .ok_or_else(|| Error::Action("unclosed !` command snippet".into()))?;
        let snippet = &after[..end];
        if snippet.contains(ARGUMENTS) {
            return Err(Error::Action(
                "command snippet cannot interpolate $ARGUMENTS".into(),
            ));
        }
        count += 1;
        if snippet.trim().is_empty() || count > 4 || snippet.len() > 4096 {
            return Err(Error::Action(
                "plugin command has empty, oversized, or too many snippets".into(),
            ));
        }
        let (program, command_args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), snippet.to_string()])
        } else {
            ("/bin/sh", vec!["-c".to_string(), snippet.to_string()])
        };
        let mut read_roots = vec![root.clone()];
        read_roots.extend(grant.read_roots(workspace));
        let mut write_roots = grant.write_roots(workspace);
        write_roots.push(data.clone());
        let request = sandbox::ExecRequest {
            program: program.into(),
            args: command_args,
            cwd: root.clone(),
            env: ["PATH", "HOME", "TMPDIR", "SystemRoot", "PATHEXT"]
                .iter()
                .filter_map(|name| {
                    std::env::var_os(name)
                        .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
                })
                .collect(),
            clear_env: true,
            read_roots,
            write_roots,
        };
        if backend.containment() != kernel::Containment::OsFsJailNoNet
            || !backend.denies_network(&request)
        {
            return Err(Error::Action(
                "network-denied plugin command sandbox is unavailable".into(),
            ));
        }
        let process = backend
            .build_plugin_command(&request, &root, &data)
            .map_err(|error| Error::Action(error.to_string()))?;
        let result = sandbox::run_command_bounded_with_input(
            process,
            Vec::new(),
            Duration::from_secs(5),
            8192,
            2048,
            None,
        )
        .await
        .map_err(|error| Error::Action(error.to_string()))?;
        if !result.passed() || result.stdout_truncated || result.stderr_truncated {
            return Err(Error::Action(
                "plugin command snippet failed, timed out, or exceeded output limits".into(),
            ));
        }
        let text = String::from_utf8(result.stdout)
            .map_err(|_| Error::Action("plugin command snippet output was not UTF-8".into()))?;
        output.push_str("[plugin command output — untrusted]\n");
        output.push_str(text.trim());
        output.push_str("\n[/plugin command output]");
        rest = &after[end + 1..];
        if output.len() > MAX_PROMPT {
            return Err(Error::Action("expanded plugin command is too large".into()));
        }
    }
    output.push_str(&rest.replace(ARGUMENTS, args));
    if !expected_prompt.contains(ARGUMENTS) && !args.is_empty() {
        output.push_str("\n\n");
        output.push_str(args);
    }
    if output.len() > MAX_PROMPT {
        return Err(Error::Action("expanded plugin command is too large".into()));
    }
    Ok(output)
}
