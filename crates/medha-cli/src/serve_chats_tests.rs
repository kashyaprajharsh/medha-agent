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
fn a_folder_just_used_is_still_open_for_the_next_request() {
    let mut kept = Kept::new(Duration::from_secs(60));
    drop(kept.get(Path::new("/asked"), || Ok(1)).unwrap());
    let second = kept.get(Path::new("/asked"), || Ok(2)).unwrap();
    assert_eq!(*second, 1, "a folder asked about twice was reopened");
}
