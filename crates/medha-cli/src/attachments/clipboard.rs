//! Reading an image out of the system clipboard.
//!
//! `arboard` covers macOS, Windows, X11 and Wayland in-process. WSL is the
//! exception: the Linux side has no access to the Windows clipboard, so a
//! screenshot taken with Win+Shift+S is invisible to it and PowerShell has to
//! hand the bytes over instead.

use super::Attachment;
use anyhow::{Context, Result, bail, ensure};
use kernel::ArtifactStore;
use std::sync::Arc;

const NO_IMAGE: &str = "the clipboard does not contain an image; copy one, or use /attach PATH";

pub async fn clipboard(store: Arc<dyn ArtifactStore>) -> Result<Vec<Attachment>> {
    tokio::task::spawn_blocking(move || {
        let png = match native_image() {
            Ok(bytes) => bytes,
            Err(error) if is_wsl() => windows_clipboard_image().map_err(|fallback| {
                fallback.context(format!("clipboard unavailable in WSL ({error})"))
            })?,
            Err(error) => return Err(error),
        };
        Ok(vec![super::admit(png, "clipboard".into(), &store)?])
    })
    .await?
}

fn native_image() -> Result<Vec<u8>> {
    let image = arboard::Clipboard::new()
        .context("cannot open the system clipboard; use /attach PATH instead")?
        .get_image()
        .context(NO_IMAGE)?;
    let (width, height) = (image.width, image.height);
    ensure!(
        width > 0
            && height > 0
            && width.checked_mul(height).and_then(|n| n.checked_mul(4)) == Some(image.bytes.len()),
        "the clipboard reported an image whose pixel buffer does not match its size"
    );
    media::from_rgba(
        u32::try_from(width).context("clipboard image is too wide")?,
        u32::try_from(height).context("clipboard image is too tall")?,
        &image.bytes,
    )
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some() || std::env::var_os("WSL_DISTRO_NAME").is_some()
}

/// PowerShell writes the clipboard image to a temporary file that both sides of
/// the WSL boundary can see, because the bytes cannot cross on stdout intact.
fn windows_clipboard_image() -> Result<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("medha-clipboard-{}.png", std::process::id()));
    let _remove = Remove(path.clone());
    let windows_path = String::from_utf8(
        std::process::Command::new("wslpath")
            .args(["-w", &path.to_string_lossy()])
            .output()
            .context("wslpath is required to read the Windows clipboard")?
            .stdout,
    )?;
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $image = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($image -eq $null) {{ exit 3 }} \
         $image.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png)",
        windows_path.trim().replace('\'', "''")
    );
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .context("powershell.exe is required to read the Windows clipboard from WSL")?;
    if status.code() == Some(3) {
        bail!(NO_IMAGE);
    }
    ensure!(status.success(), "PowerShell could not read the clipboard");
    std::fs::read(&path).context("PowerShell reported success but wrote no image")
}

struct Remove(std::path::PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
