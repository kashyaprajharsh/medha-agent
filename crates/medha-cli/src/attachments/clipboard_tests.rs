use super::*;

fn no_image() -> Result<Vec<u8>> {
    bail!(NO_IMAGE)
}

/// Ctrl+V with copied text must paste it, not complain that it is no image.
#[test]
fn text_is_pasted_when_the_clipboard_holds_no_image() {
    match choose(no_image(), || Some("sk-copied".into())).unwrap() {
        Contents::Text(text) => assert_eq!(text, "sk-copied"),
        Contents::Image(_) => panic!("text came back as an image"),
    }
}

#[test]
fn an_image_wins_over_the_text_copied_with_it() {
    let read = choose(Ok(vec![1, 2, 3]), || {
        panic!("text read although an image was there")
    });
    assert!(matches!(read.unwrap(), Contents::Image(bytes) if bytes == [1, 2, 3]));
}

#[test]
fn an_empty_clipboard_says_why_nothing_was_pasted() {
    let error = choose(no_image(), || None).err().expect("nothing to paste");
    assert!(
        error.to_string().contains("does not contain an image"),
        "{error}"
    );
}
