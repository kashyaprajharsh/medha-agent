use super::parse_osc11_luma;

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
