use super::*;

fn image(label: &str) -> Attachment {
    Attachment {
        part: kernel::MediaPart {
            mime_type: "image/png".into(),
            source: kernel::MediaSource::Artifact(label.into()),
            label: None,
            provider_state: Vec::new(),
        },
        label: label.into(),
        source: Some(std::path::PathBuf::from("/work").join(label)),
        width: 800,
        height: 600,
        bytes: 2048,
        note: None,
    }
}

fn parts(labels: &[&str]) -> Vec<kernel::MediaPart> {
    labels.iter().map(|label| image(label).part).collect()
}

#[test]
fn staged_images_ride_with_the_next_message_once() {
    let mut pending = PendingImages::default();
    let generation = pending.begin().unwrap();
    assert!(pending.is_loading());
    assert!(
        pending
            .accept(generation, Ok(vec![image("shot.png")]))
            .is_some()
    );
    assert!(!pending.is_loading());
    assert_eq!(pending.take(), parts(&["shot.png"]));
    assert!(pending.is_empty());
    assert!(pending.take().is_empty());
}

#[test]
fn a_load_that_outlives_its_session_is_dropped() {
    let mut pending = PendingImages::default();
    let stale = pending.begin().unwrap();
    pending.reset();
    assert_eq!(pending.accept(stale, Ok(vec![image("shot.png")])), None);
    assert!(pending.is_empty() && !pending.is_loading());

    let sent = pending.begin().unwrap();
    pending.accept(sent, Ok(vec![image("shot.png")]));
    pending.take();
    assert_eq!(pending.accept(sent, Ok(vec![image("late.png")])), None);
    assert!(pending.is_empty());
}

#[test]
fn refuses_a_second_load_and_a_fifth_image() {
    let mut pending = PendingImages::default();
    let generation = pending.begin().unwrap();
    assert!(pending.begin().is_err());
    pending.accept(
        generation,
        Ok((0..MAX_PER_MESSAGE)
            .map(|n| image(&n.to_string()))
            .collect()),
    );
    assert!(pending.begin().is_err());

    let mut over = PendingImages::default();
    let generation = over.begin().unwrap();
    over.accept(generation, Ok(vec![image("one.png")]));
    let generation = over.begin().unwrap();
    let notice = over
        .accept(
            generation,
            Ok((0..MAX_PER_MESSAGE)
                .map(|n| image(&n.to_string()))
                .collect()),
        )
        .unwrap();
    assert!(notice.contains("at most"), "{notice}");
    assert_eq!(over.take(), parts(&["one.png"]));
}

#[test]
fn a_failed_load_releases_the_slot() {
    let mut pending = PendingImages::default();
    let generation = pending.begin().unwrap();
    let notice = pending
        .accept(generation, Err("not an image".into()))
        .unwrap();
    assert!(notice.contains("not an image"), "{notice}");
    assert!(!pending.is_loading());
    assert!(pending.begin().is_ok());
}

#[test]
fn the_composer_shows_what_admission_changed() {
    let mut pending = PendingImages::default();
    let generation = pending.begin().unwrap();
    let mut converted = image("scan.tiff");
    converted.note = Some("image/tiff → image/png".into());
    let notice = pending.accept(generation, Ok(vec![converted])).unwrap();
    assert!(notice.contains("image/tiff → image/png"), "{notice}");
    let title = pending.title().unwrap();
    assert!(title.contains("1. scan.tiff  800×600 · 2 KB"), "{title}");
}

#[test]
fn detach_addresses_the_numbers_shown_on_the_composer() {
    let mut pending = PendingImages::default();
    for name in ["one.png", "two.png"] {
        let generation = pending.begin().unwrap();
        pending.accept(generation, Ok(vec![image(name)]));
    }
    let title = pending.title().unwrap();
    assert!(
        title.contains("1. one.png") && title.contains("2. two.png"),
        "{title}"
    );
    pending.detach("1");
    assert!(pending.title().unwrap().contains("1. two.png"));
    assert!(pending.detach("9").contains("no attachment"));
    assert!(pending.detach("first").contains("usage"));
    pending.detach("all");
    assert!(pending.is_empty() && pending.title().is_none());
}

#[test]
fn the_transcript_records_what_the_turn_carried() {
    let mut pending = PendingImages::default();
    assert_eq!(pending.submission_label("hello"), "hello");
    let generation = pending.begin().unwrap();
    pending.accept(generation, Ok(vec![image("shot.png")]));
    assert_eq!(
        pending.submission_label("what is this?"),
        "what is this?\n[attached: shot.png]"
    );
}
