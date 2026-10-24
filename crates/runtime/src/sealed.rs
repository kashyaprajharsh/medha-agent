//! Keys too long for Windows Credential Manager, kept in the credentials file
//! but sealed with DPAPI so only the signed-in Windows user can open them.

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;

/// Marks a stored value that only the Windows user who sealed it can open.
const SEALED: &str = "sealed:v1:";

/// The key as stored, opened first when it was sealed.
pub(crate) fn unseal(stored: &str) -> Result<String> {
    let Some(sealed) = stored.strip_prefix(SEALED) else {
        return Ok(stored.to_owned());
    };
    let bytes = STANDARD
        .decode(sealed)
        .context("a sealed key in the credentials file is damaged")?;
    let key = String::from_utf8(dpapi(&bytes, false)?)
        .context("a sealed key in the credentials file is damaged")?;
    // The file read registered only the sealed text, so the real key is hidden from transcripts here.
    kernel::redact::remember_stored(&key);
    Ok(key)
}

/// Opens every sealed key, for a person who chose to keep keys in the file.
pub(crate) fn unseal_all(keys: &mut BTreeMap<String, String>) -> Result<()> {
    for key in keys.values_mut() {
        *key = unseal(key)?;
    }
    Ok(())
}

/// Seals every key the keychain could not hold; only Windows has such a limit.
pub(crate) fn seal_kept(keys: &mut BTreeMap<String, String>) -> Result<()> {
    #[cfg(windows)]
    for key in keys.values_mut() {
        if !key.starts_with(SEALED) {
            *key = seal(key)?;
        }
    }
    #[cfg(not(windows))]
    let _ = keys;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn seal(plain: &str) -> Result<String> {
    Ok(format!(
        "{SEALED}{}",
        STANDARD.encode(dpapi(plain.as_bytes(), true)?)
    ))
}

/// Seals or opens bytes with the current user's DPAPI key, never the machine's.
#[cfg(windows)]
fn dpapi(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };

    let data = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).context("a key is too large to seal")?,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    let done = unsafe {
        if protect {
            CryptProtectData(
                &data,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &data,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if done == 0 {
        return Err(std::io::Error::last_os_error()).context(if protect {
            "Windows could not seal the key"
        } else {
            "Windows could not open a sealed key; it opens only for the user who saved it"
        });
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData.cast()) };
    Ok(bytes)
}

#[cfg(not(windows))]
fn dpapi(_: &[u8], _: bool) -> Result<Vec<u8>> {
    anyhow::bail!("this key was sealed on Windows and opens only there")
}

#[cfg(test)]
#[path = "sealed_tests.rs"]
mod tests;
