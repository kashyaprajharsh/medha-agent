use super::project_root;
use crate::ctxfiles::ContextFileLoader;

#[tokio::test]
async fn instruction_discovery_uses_the_same_home_and_nested_checkout_boundaries() {
    let files = test_support::scratch("instruction-roots");
    let home = files.join("home");
    let profile = home.join(".medha");
    std::fs::create_dir_all(home.join(".git")).unwrap();
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(home.join("AGENTS.md"), "HOME_IS_NOT_A_PROJECT").unwrap();
    let loader = ContextFileLoader::new();
    assert_eq!(project_root(&home, &home).unwrap(), None);
    assert!(
        loader
            .discover_startup_in(&home, &profile, &home)
            .await
            .is_empty()
    );
    let outer = files.join("outer");
    let nested = outer.join("vendor/inner");
    let child = nested.join("src");
    std::fs::create_dir_all(outer.join(".git")).unwrap();
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(nested.join(".git"), "gitdir: ../../.git/modules/inner").unwrap();
    std::fs::write(outer.join("AGENTS.md"), "OUTER_CHECKOUT").unwrap();
    std::fs::write(nested.join("AGENTS.md"), "INNER_CHECKOUT").unwrap();
    assert_eq!(
        project_root(&child, &home).unwrap(),
        Some(nested.canonicalize().unwrap())
    );
    let discovered = loader.discover_startup_in(&child, &profile, &home).await;
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].content, "INNER_CHECKOUT");
    #[cfg(unix)]
    {
        let alias = files.join("alias");
        std::os::unix::fs::symlink(&child, &alias).unwrap();
        assert_eq!(
            project_root(&alias, &home).unwrap(),
            project_root(&child, &home).unwrap()
        );
        assert_eq!(
            loader.discover_startup_in(&alias, &profile, &home).await[0].path,
            discovered[0].path
        );
    }
}
