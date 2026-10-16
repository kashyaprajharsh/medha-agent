use kernel::context::RequestContext;
use runtime::project_session::{LiveContext, LiveSettings};
use runtime::project_trust::ProjectTrust;
use std::collections::HashSet;
use std::sync::Arc;

#[test]
#[ignore = "measures project refresh against a real checkout and installed skill packages"]
fn measure_real_project_refresh() {
    let repo = std::env::var_os("MEDHA_BENCH_PROJECT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .canonicalize()
        .unwrap();
    let skills = std::env::var_os("MEDHA_BENCH_SKILLS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".medha/skills"))
        .canonicalize()
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    // This ignored benchmark runs alone and leaves the user's saved trust untouched.
    unsafe {
        std::env::set_var("MEDHA_HOME", state.path());
    }
    let trust = ProjectTrust::for_folder(&repo).unwrap().unwrap();
    trust.accept(&trust.review(&repo).unwrap()).unwrap();
    let settings = LiveSettings::new(
        repo.clone(),
        runtime::SessionOptions::default(),
        runtime::workspace::load_lock(&repo, &Quiet).unwrap(),
        sandbox::HomeScope::default(),
        sandbox::ApprovedRoots::default(),
        sandbox::NetworkGrant::default(),
        None,
    );
    let cache = state.path().join("scans");
    let store = Arc::new(
        tools::SkillStore::new(skills.clone(), None)
            .with_project_source(Arc::new(move |_, _| Some(cache.clone()))),
    );
    let known: HashSet<_> = ["skill", "read", "write", "edit", "shell.exec", "web", "lsp"]
        .into_iter()
        .map(String::from)
        .collect();
    let packages = std::fs::read_dir(&skills)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .count();
    assert!(packages >= 3, "benchmark needs several real skill packages");
    let mut prefix = context::identity::system_prompt_for_tools(None, &known);
    prefix.push_str(&format!(
        "\n\nEnvironment:\n- Workspace: {}",
        repo.display()
    ));
    let live = LiveContext::new(settings, store.clone(), known.clone(), None, None);
    live.bind_project_slot(context::identity::ProjectSlot::after(&prefix));
    let mut system = live.replace_memory("Benchmark memory");
    let mut samples = Vec::new();
    let mut continuations = Vec::new();
    let start = std::time::Instant::now();
    live.begin_message(&[], &Quiet);
    system = live.refresh(&system, &Quiet);
    let first = start.elapsed();
    for index in 0..34 {
        let start = std::time::Instant::now();
        live.begin_message(&[], &Quiet);
        system = live.refresh(&system, &Quiet);
        if index >= 3 {
            samples.push(start.elapsed());
        }
        for _ in 0..8 {
            let start = std::time::Instant::now();
            system = live.refresh(&system, &Quiet);
            if index >= 3 {
                continuations.push(start.elapsed());
            }
        }
    }
    samples.sort();
    continuations.sort();
    println!(
        "repo={}, skill_source={}, packages={}, usable={}, cold_message={} us, person_message median={} us p95={} us",
        repo.display(),
        skills.display(),
        packages,
        store.discover(&known).effective().count(),
        first.as_micros(),
        samples[15].as_micros(),
        samples[29].as_micros()
    );
    println!(
        "model continuation median={} ns p95={} ns",
        continuations[continuations.len() / 2].as_nanos(),
        continuations[continuations.len() * 95 / 100].as_nanos()
    );
}

struct Quiet;
impl runtime::Notices for Quiet {
    fn say(&self, _: &str) {}
}
impl kernel::StreamSink for Quiet {}
