use super::Seam;

/// Feed a continuation a few characters at a time, as a stream delivers it.
fn joined(before: &str, continuation: &str) -> (String, usize) {
    let mut seam = Seam::after(before);
    let pieces: Vec<char> = continuation.chars().collect();
    let mut shown = String::new();
    for piece in pieces.chunks(7) {
        shown.push_str(&seam.feed(&piece.iter().collect::<String>()));
    }
    shown.push_str(&seam.finish());
    (shown, seam.cut)
}

/// A continuation that begins by repeating the end of what was written loses
/// exactly that repeat. One that simply goes on, shares only a few words, or
/// carries on a rule or an indent, loses nothing.
#[test]
fn only_a_real_repeat_at_the_join_is_cut() {
    let before = "The parser keeps a stack of open blocks. When a block closes it is folded \
        into its parent, and the parent's span";
    let tail = "it is folded into its parent, and the parent's span";

    let (shown, cut) = joined(before, &format!("{tail} is widened to cover it."));
    assert_eq!(shown, " is widened to cover it.");
    assert_eq!(cut, tail.chars().count());

    // Going straight on: everything is shown.
    let next = " is widened to cover it. Errors are collected, not thrown.";
    assert_eq!(joined(before, next), (next.to_owned(), 0));

    // A few shared words are ordinary writing, not a repeat.
    let short = "the parent's span is widened";
    assert_eq!(joined(before, short), (short.to_owned(), 0));

    // Text that appeared earlier but is not the end of what was written stays.
    let earlier = "The parser keeps a stack of open blocks. It also keeps a list.";
    assert_eq!(joined(before, earlier), (earlier.to_owned(), 0));

    // The whole of what was written, repeated, and then the stream ends.
    assert_eq!(
        joined(before, before),
        (String::new(), before.chars().count())
    );

    // Not enough had been written to tell a repeat from writing.
    assert_eq!(
        joined("Yes.", "Yes. And more."),
        ("Yes. And more.".to_owned(), 0)
    );

    // A rule that was cut in the middle goes on with more of itself: all of it stays.
    let rule = format!("Results\n{}", "-".repeat(40));
    let more = format!("{}\nDone.", "-".repeat(36));
    assert_eq!(joined(&rule, &more), (more.clone(), 0));

    // Counted in characters, not bytes: text outside ASCII is cut cleanly.
    let before = "नमस्ते, यह उत्तर अभी पूरा नहीं हुआ है और आगे चलता है";
    let (shown, cut) = joined(before, &format!("{before} — आगे।"));
    assert_eq!((shown.as_str(), cut), (" — आगे।", before.chars().count()));

    // A long reply, as the ones that get cut off are: its end is still read.
    let long = format!("{}{tail}", "A sentence that fills the page. ".repeat(600));
    let (shown, cut) = joined(&long, &format!("{tail} and more."));
    assert_eq!((shown.as_str(), cut), (" and more.", tail.chars().count()));
}

/// A stream that dies while the start is still held has shown none of it,
/// and what it held is only text that was already written.
#[test]
fn what_is_still_held_when_a_stream_dies_was_already_written() {
    let before = "The parser keeps a stack of open blocks. When a block closes it is folded.";
    let mut seam = Seam::after(before);
    assert_eq!(seam.feed("When a block clo"), "");
    assert_eq!(seam.hidden(), 16);
    assert!(before.contains("When a block clo"));
}
