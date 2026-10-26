//! Systems with no service manager Medha knows: the gateway runs under the user's own supervisor.

use anyhow::{Result, bail};
use std::path::Path;

const ADVICE: &str = "Starting the gateway at login is not available on this system. Run `medha gateway run` under your own supervisor (tmux, a container restart policy).";

pub(super) fn register(_program: &Path) -> Result<String> {
    bail!(ADVICE)
}

pub(super) fn unregister() -> Result<()> {
    bail!(ADVICE)
}

pub(super) fn registered() -> Option<String> {
    None
}

pub(super) fn start() -> Result<()> {
    bail!(ADVICE)
}

pub(super) fn stop() -> Result<()> {
    bail!(ADVICE)
}

pub(super) fn restart() -> Result<()> {
    bail!(ADVICE)
}
