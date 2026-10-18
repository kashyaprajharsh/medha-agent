//! Shared runnable-file classification and operating-system opening.

use std::{io::Read, path::Path, process::Command};

pub fn runnable(path: &Path) -> std::io::Result<bool> {
    let path = path.canonicalize()?;
    let metadata = std::fs::metadata(&path)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ["app", "bundle", "workflow", "scptd", "plugin"].contains(&extension.as_str()) {
        return Ok(true);
    }
    if metadata.is_dir() {
        return Ok(false);
    }
    if !metadata.is_file() {
        return Err(std::io::Error::other(
            "Only regular files and directories can be opened",
        ));
    }
    if [
        "exe",
        "com",
        "msi",
        "msp",
        "scr",
        "pif",
        "bat",
        "cmd",
        "ps1",
        "psm1",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "lnk",
        "url",
        "appref-ms",
        "command",
        "sh",
        "bash",
        "zsh",
        "fish",
        "py",
        "pyw",
        "rb",
        "pl",
        "php",
        "lua",
        "jar",
        "class",
        "desktop",
        "run",
        "scpt",
    ]
    .contains(&extension.as_str())
    {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return Ok(true);
        }
    }
    let mut prefix = [0u8; 4];
    let size = std::fs::File::open(path)?.read(&mut prefix)?;
    Ok(prefix[..size].starts_with(b"#!")
        || prefix[..size].starts_with(b"MZ")
        || prefix == *b"\x7fELF"
        || matches!(
            prefix,
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
        ))
}

pub fn open_file(path: &Path) -> Result<Option<String>, String> {
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    if runnable(&path).map_err(|error| error.to_string())? {
        reveal(&path)?;
        Ok(Some(
            "Revealed in folder because this file can run code. It was not launched.".into(),
        ))
    } else {
        open_path(&path)?;
        Ok(None)
    }
}

pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(not(any(target_os = "macos", windows)))]
    let program = "xdg-open";
    Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(format!("/select,{}", path.display()));
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path.parent().ok_or("File has no parent folder")?);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "opening_tests.rs"]
mod tests;
