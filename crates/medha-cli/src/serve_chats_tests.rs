use super::*;

#[test]
fn a_folder_nobody_holds_closes_and_one_a_chat_holds_is_never_reopened() {
    let mut kept = Kept::new(Duration::ZERO);
    let (held_folder, unused_folder) = (Path::new("/held"), Path::new("/unused"));

    let chat = kept.get(held_folder, || Ok("first")).unwrap();
    let unused = Arc::downgrade(&kept.get(unused_folder, || Ok("first")).unwrap());

    let again = kept.get(held_folder, || Ok("reopened")).unwrap();
    assert!(
        Arc::ptr_eq(&chat, &again),
        "a folder in use was opened twice"
    );
    assert!(unused.upgrade().is_none(), "an unused folder stayed open");
}

#[test]
fn an_idle_backend_lets_go_of_folders_without_being_asked_anything_more() {
    let mut kept = Kept::new(Duration::from_secs(60));
    let chat = kept.get(Path::new("/held"), || Ok(0)).unwrap();
    let unused = Arc::downgrade(&kept.get(Path::new("/unused"), || Ok(0)).unwrap());

    kept.sweep(Instant::now() + Duration::from_secs(30));
    assert!(unused.upgrade().is_some(), "it closed before it was idle");
    kept.sweep(Instant::now() + Duration::from_secs(600));
    assert!(
        unused.upgrade().is_none(),
        "nothing closed it but another request"
    );
    assert_eq!(
        Arc::strong_count(&chat),
        2,
        "a folder a chat holds was closed"
    );
}

#[test]
fn only_so_many_folders_nobody_holds_stay_open_and_those_the_latest_used() {
    let mut kept = Kept::new(Duration::from_secs(3600));
    let folders: Vec<PathBuf> = (0..UNHELD + 5)
        .map(|n| PathBuf::from(format!("/visited-{n}")))
        .collect();
    let handles: Vec<_> = folders
        .iter()
        .map(|folder| {
            std::thread::sleep(Duration::from_millis(1));
            Arc::downgrade(&kept.get(folder, || Ok(0)).unwrap())
        })
        .collect();
    kept.sweep(Instant::now());
    let open: Vec<bool> = handles
        .iter()
        .map(|folder| folder.upgrade().is_some())
        .collect();
    assert_eq!(open.iter().filter(|open| **open).count(), UNHELD);
    assert!(
        open[5..].iter().all(|open| *open),
        "a later folder closed before an earlier one"
    );
}

#[test]
fn a_folder_just_used_is_still_open_for_the_next_request() {
    let mut kept = Kept::new(Duration::from_secs(60));
    drop(kept.get(Path::new("/asked"), || Ok(1)).unwrap());
    let second = kept.get(Path::new("/asked"), || Ok(2)).unwrap();
    assert_eq!(*second, 1, "a folder asked about twice was reopened");
}
