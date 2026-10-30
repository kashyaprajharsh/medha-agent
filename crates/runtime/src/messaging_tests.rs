use super::*;

#[test]
fn a_phone_reaches_a_project_never_the_home_folder_a_root_or_medhas_own_data() {
    let project = tempfile::tempdir().unwrap();
    let admitted = admit_folder(project.path()).unwrap();
    assert_eq!(admitted, project.path().canonicalize().unwrap());

    let home = dirs::home_dir().unwrap();
    assert!(admit_folder(&home).is_err(), "the home folder");
    assert!(
        admit_folder(home.parent().unwrap()).is_err(),
        "a folder above home"
    );
    assert!(admit_folder(std::path::Path::new("/")).is_err(), "the root");
    assert!(
        admit_folder(&project.path().join("missing")).is_err(),
        "a missing folder"
    );
    let medha = crate::config::medha_home().unwrap();
    assert!(admit_folder(&medha).is_err(), "Medha's own data");
    assert!(
        admit_folder(&medha.join("projects")).is_err(),
        "inside Medha's data"
    );
}

#[test]
fn a_new_default_keeps_the_old_one_open_to_chats_that_chose_it() {
    let mut messaging = Messaging {
        folder: Some("/work/medha".into()),
        folders: vec!["/work/site".into()],
        ..Messaging::default()
    };
    messaging.make_default("/work/site".into());
    assert_eq!(
        messaging.allowed(),
        [PathBuf::from("/work/site"), PathBuf::from("/work/medha")]
    );
    messaging.make_default("/work/site".into());
    assert_eq!(
        messaging.allowed().len(),
        2,
        "setting the same default twice adds nothing"
    );
}

#[test]
fn the_default_comes_first_and_no_folder_is_offered_twice() {
    let messaging = Messaging {
        folder: Some("/work/medha".into()),
        folders: vec![
            "/work/site".into(),
            "/work/medha".into(),
            "/work/site".into(),
        ],
        ..Messaging::default()
    };
    assert_eq!(
        messaging.allowed(),
        [PathBuf::from("/work/medha"), PathBuf::from("/work/site")]
    );
}
