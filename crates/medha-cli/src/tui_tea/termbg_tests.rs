use super::{parse_osc11_luma, reply_range};

#[test]
fn osc11_replies_parse_across_terminal_dialects() {
    let white = parse_osc11_luma("\x1b]11;rgb:ffff/ffff/ffff\x07").unwrap();
    assert!(white > 0.99, "white parsed as {white}");

    let black = parse_osc11_luma("\x1b]11;rgb:0000/0000/0000\x1b\\").unwrap();
    assert!(black < 0.01, "black parsed as {black}");

    // Some terminals answer 2 nibbles per channel rather than xterm's 4.
    let short = parse_osc11_luma("\x1b]11;rgb:ff/ff/ff\x07").unwrap();
    assert!(short > 0.99, "8-bit white parsed as {short}");

    let parchment = parse_osc11_luma("\x1b]11;rgb:f9f9/f6f6/efef\x07").unwrap();
    assert!(parchment > 0.5, "parchment parsed as {parchment}");
}

#[test]
fn a_reply_that_cannot_be_parsed_declines_rather_than_guessing() {
    assert!(parse_osc11_luma("").is_none());
    assert!(parse_osc11_luma("\x1b]11;not-a-colour\x07").is_none());
    assert!(parse_osc11_luma("\x1b]11;rgb:ffff/ffff\x07").is_none());
}

/// The Windows probe removes exactly these bytes from the console queue; everything else goes back.
#[test]
fn only_the_reply_is_taken_from_what_was_typed_around_it() {
    let reply = b"\x1b]11;rgb:0c0c/0c0c/0c0c\x1b\\";
    let typed = [b"fix ".as_slice(), reply, b"tests"].concat();
    let range = reply_range(&typed).unwrap();
    assert_eq!(&typed[range.clone()], reply);
    let kept: Vec<u8> = typed
        .iter()
        .enumerate()
        .filter(|(at, _)| !range.contains(at))
        .map(|(_, byte)| *byte)
        .collect();
    assert_eq!(kept, b"fix tests");

    let bell = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
    assert_eq!(reply_range(bell), Some(0..bell.len()));
}

/// Pasted text that looks like a reply belongs to the person and must reach the input box.
#[test]
fn a_reply_inside_a_paste_is_left_for_the_input_box() {
    let pasted = b"\x1b[200~\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[201~";
    assert_eq!(reply_range(pasted), None);
    let after = [pasted.as_slice(), b"\x1b]11;rgb:0000/0000/0000\x07"].concat();
    assert_eq!(reply_range(&after), Some(pasted.len()..after.len()));
}

/// Half a reply, or one that is not a colour, is never removed: putting it back loses nothing.
#[test]
fn an_unfinished_or_unreadable_reply_is_not_taken() {
    assert_eq!(reply_range(b"\x1b]11;rgb:0c0c/0c0c"), None);
    assert_eq!(reply_range(b"\x1b]11;not-a-colour\x07"), None);
    assert_eq!(
        reply_range(b"\x1b[200~unclosed \x1b]11;rgb:0/0/0\x07"),
        None
    );
}
