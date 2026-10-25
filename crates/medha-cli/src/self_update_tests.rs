use super::*;
use std::ffi::OsStr;

#[test]
fn only_a_newer_release_is_offered() {
    assert!(newer("v0.1.13", "0.1.12"));
    assert!(newer("v0.2.0", "0.1.99"));
    assert!(!newer("v0.1.12", "0.1.12"));
    assert!(!newer("v0.1.11", "0.1.12"));
    // A prerelease or a tag that is not a version never replaces a release.
    for tag in ["v0.1.13-rc.1", "latest", "v0.1", "v0.1.13.1", ""] {
        assert!(!newer(tag, "0.1.12"), "{tag}");
    }
}

#[test]
fn only_a_copy_in_an_installers_version_folder_can_update_itself() {
    let root = Path::new("/home/me/.local/lib/medha");
    let install = Install::from_exe(&root.join("versions/v0.1.12/medha")).unwrap();
    assert_eq!(install.root, root);
    assert_eq!(
        install.executable("v0.1.13"),
        root.join("versions")
            .join("v0.1.13")
            .join(if cfg!(windows) { "medha.exe" } else { "medha" })
    );
    for elsewhere in [
        "/home/me/project/target/release/medha",
        "/home/me/.cargo/bin/medha",
        "/home/me/.local/lib/medha/versions/latest/medha",
    ] {
        assert_eq!(Install::from_exe(Path::new(elsewhere)), None, "{elsewhere}");
    }
}

#[test]
fn the_installer_is_pinned_to_the_offered_release_and_the_install_folder() {
    let env = |command: &std::process::Command, key: &str| {
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(key))
            .map(|(_, value)| value.map(OsStr::to_os_string))
    };
    let folder = PathBuf::from("/opt/me/bin");
    let install = Install {
        root: PathBuf::from("/opt/me/lib/medha"),
        install_dir: Some(folder.clone()),
    };
    let command = installer(Path::new("/tmp/x/install.sh"), "v0.1.13", &install);
    assert_eq!(env(&command, "MEDHA_VERSION"), Some(Some("v0.1.13".into())));
    assert_eq!(
        env(&command, "MEDHA_INSTALL_DIR"),
        Some(Some(folder.into()))
    );
    let program = PathBuf::from(command.get_program());
    if cfg!(windows) {
        assert!(program.is_absolute() && program.ends_with("powershell.exe"));
    } else {
        assert_eq!(program, Path::new("/bin/sh"));
    }
    // Without a known folder the installer chooses, and an inherited override is not passed on.
    let install = Install {
        install_dir: None,
        ..install
    };
    let command = installer(Path::new("/tmp/x/install.sh"), "v0.1.13", &install);
    assert_eq!(env(&command, "MEDHA_INSTALL_DIR"), Some(None));
}

#[test]
fn a_skipped_release_is_not_offered_again_but_the_next_one_is() {
    let cache = |latest: &str, skipped: Option<&str>| Cache {
        latest: latest.into(),
        checked_at: 0,
        skipped: skipped.map(str::to_owned),
    };
    assert_eq!(
        offered(&cache("v0.1.13", None), "0.1.12").as_deref(),
        Some("v0.1.13")
    );
    assert_eq!(offered(&cache("v0.1.13", Some("v0.1.13")), "0.1.12"), None);
    assert_eq!(
        offered(&cache("v0.1.14", Some("v0.1.13")), "0.1.12").as_deref(),
        Some("v0.1.14")
    );
    assert_eq!(offered(&cache("v0.1.12", None), "0.1.12"), None);
}
