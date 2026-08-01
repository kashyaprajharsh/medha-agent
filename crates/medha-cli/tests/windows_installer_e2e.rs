#![cfg(windows)]

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn executable_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn run_checks(shell: &Path) {
    let temp = TempDir::new().unwrap();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checks = manifest.join("tests/windows_installer_checks.ps1");
    let installer = manifest.join("../../install.ps1");
    let output = Command::new(shell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive"])
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(checks)
        .arg("-Installer")
        .arg(installer)
        .arg("-TempRoot")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} installer checks failed:\nstdout:\n{}\nstderr:\n{}",
        shell.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn windows_powershell_installers_parse_and_validate_fail_closed() {
    let windows_powershell =
        executable_on_path("powershell.exe").expect("Windows PowerShell 5 is required");
    run_checks(&windows_powershell);

    // GitHub's supported Windows release runner includes PowerShell 7. Keep
    // local Windows development usable when only the in-box shell is present.
    if let Some(power_shell_7) = executable_on_path("pwsh.exe") {
        run_checks(&power_shell_7);
    }
}
