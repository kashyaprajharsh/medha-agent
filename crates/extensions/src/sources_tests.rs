use super::*;
use crate::test_support::store;
use crate::{Activation, Scope};
use std::fs;

#[test]
fn specs_cover_folders_github_refs_urls_and_marketplace_names() {
    assert_eq!(
        parse("dietrichgebert/ponytail"),
        Spec::Git(GitSource {
            url: "https://github.com/dietrichgebert/ponytail".into(),
            git_ref: None,
            subdir: None,
        })
    );
    let Spec::Git(pinned) = parse("https://github.com/o/r/tree/v2/plugins/x") else {
        panic!("expected git");
    };
    assert_eq!(pinned.git_ref.as_deref(), Some("v2"));
    assert_eq!(pinned.subdir.as_deref(), Some("plugins/x"));
    let Spec::Git(tagged) = parse("o/r@v4.10.0") else {
        panic!("expected git");
    };
    assert_eq!(tagged.git_ref.as_deref(), Some("v4.10.0"));
    assert_eq!(tagged.owner().as_deref(), Some("o"));
    assert_eq!(
        parse("ponytail@ponytail"),
        Spec::Listed {
            plugin: "ponytail".into(),
            marketplace: "ponytail".into(),
        }
    );
    assert!(matches!(parse("./local/plugin"), Spec::Local(_)));
    assert!(matches!(parse("git@github.com:o/r.git"), Spec::Git(_)));
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

use crate::compat::write_community_plugin as write_plugin;

#[test]
fn a_repository_installs_pinned_updates_with_an_access_diff_and_rolls_back() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    write_plugin(&repo, "1.0.0", false);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);
    let url = format!("file://{}", repo.display());

    let markets = Marketplaces::new(temp.path().join("markets.toml"));
    let market = markets
        .add(match parse(&url) {
            Spec::Git(source) => source,
            other => panic!("{other:?}"),
        })
        .unwrap();
    assert_eq!(market.name, "tidy-market");
    assert_eq!(market.plugins[0].name, "tidy");

    let store = store(temp.path());
    let installed = store.install_from("tidy@tidy-market", &markets).unwrap();
    assert_eq!(installed.manifest.id, "tidy-market.tidy");
    assert_eq!(installed.manifest.version, "1.0.0");
    let plugin = store.inspect("tidy-market.tidy", None).unwrap();
    assert_eq!(plugin.activation, Activation::Disabled);
    assert!(plugin.package.manifest.components.iter().any(
        |component| matches!(component, crate::ExtensionComponent::Skill { id, .. } if id == "tidy")
    ));
    store
        .enable("tidy-market.tidy", None, &plugin.requested_grant().unwrap())
        .unwrap();

    let plan = store.plan_update("tidy-market.tidy").unwrap();
    assert!(plan.up_to_date());

    write_plugin(&repo, "2.0.0", true);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "two"]);
    let plan = store.plan_update("tidy-market.tidy").unwrap();
    assert!(!plan.up_to_date());
    assert_eq!(plan.to_version, "2.0.0");
    assert!(plan.access_changed(), "the new MCP server needs network");
    let updated = store.apply_update(plan, false).unwrap();
    assert_eq!(updated.manifest.version, "2.0.0");
    assert_eq!(
        store.inspect("tidy-market.tidy", None).unwrap().activation,
        Activation::Disabled,
        "new access is not approved silently"
    );

    let restored = store.rollback("tidy-market.tidy").unwrap();
    assert_eq!(restored.manifest.version, "1.0.0");
    let plugin = store
        .inspect("tidy-market.tidy", Some(Scope::User))
        .unwrap();
    assert_eq!(
        plugin.activation,
        Activation::Enabled,
        "rollback restores the old approval"
    );

    store.remove("tidy-market.tidy").unwrap();
    assert!(!store.previous_dir("tidy-market.tidy").exists());
}

fn committed_repo(dir: &Path) -> GitSource {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet"]);
    write_plugin(dir, "1.0.0", false);
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "one"]);
    match parse(&format!("file://{}", dir.display())) {
        Spec::Git(source) => source,
        other => panic!("{other:?}"),
    }
}

#[test]
fn default_catalogs_are_added_once_and_a_removed_one_stays_removed() {
    let temp = tempfile::tempdir().unwrap();
    let source = committed_repo(&temp.path().join("repo"));
    let markets =
        Marketplaces::new(temp.path().join("markets.toml")).with_defaults(vec![source.clone()]);
    assert_eq!(markets.pending_defaults(), std::slice::from_ref(&source));

    let store = store(temp.path());
    let installed = store.install_from("tidy@tidy-market", &markets).unwrap();
    assert_eq!(
        installed.manifest.id, "tidy-market.tidy",
        "installs seed defaults"
    );
    assert!(markets.pending_defaults().is_empty());

    markets.remove("tidy-market").unwrap();
    assert!(markets.pending_defaults().is_empty());
    assert!(markets.add_defaults().is_empty());
    assert!(markets.list().unwrap().is_empty());
}

#[test]
fn fetched_links_inside_the_repository_become_copies_and_others_are_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".github")).unwrap();
    fs::write(temp.path().join("secret.txt"), "outside").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../skills/tidy", repo.join(".github/tidy")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("secret.txt"), repo.join("leak.txt")).unwrap();
    }
    let source = committed_repo(&repo);
    let checkout = fetch(&source).unwrap();
    let copied = checkout.dir().join(".github/tidy");
    assert!(
        !fs::symlink_metadata(&copied)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(copied.join("SKILL.md").is_file());
    assert!(!checkout.dir().join("leak.txt").exists());
    let markets = Marketplaces::new(temp.path().join("markets.toml"));
    store(temp.path())
        .install_from(&format!("file://{}", repo.display()), &markets)
        .unwrap();
}
