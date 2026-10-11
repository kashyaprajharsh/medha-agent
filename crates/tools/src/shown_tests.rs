use super::*;

fn shown(start: u64, lines: &[&str]) -> ShownPages {
    let pages = ShownPages::default();
    pages.remember(start, lines.iter().map(|line| (*line).to_owned()).collect());
    pages
}

/// Read output pasted back is recognised by what was actually shown, so it is
/// caught even inside other text, and its numbers alone are taken off.
#[test]
fn lines_copied_from_a_shown_page_are_recognised_and_cleaned() {
    let pages = shown(41, &["fn main() {", "    run();", "}"]);
    assert_eq!(
        pages
            .pasted("// start\n41|fn main() {\n42|    run();\r\n43|}\n")
            .as_deref(),
        Some("// start\nfn main() {\n    run();\r\n}\n")
    );
    // One copied line is not enough to call it a paste.
    assert!(pages.pasted("41|fn main() {\nlet x = 1;\n").is_none());
    // Numbers that do not follow on are two separate lines, not a copy.
    assert!(pages.pasted("41|fn main() {\n43|}\n").is_none());
}

/// A file whose own lines start with a number and a bar is ordinary content:
/// nothing shown reads that way under those numbers, even after reading the
/// very same file, where each line was shown with a number in front of its own.
#[test]
fn a_file_of_numbered_rows_is_never_mistaken_for_read_output() {
    let rows = "1|alice\n2|bob\n3|carol\n";
    assert!(ShownPages::default().pasted(rows).is_none());
    let after_reading_it = shown(1, &["1|alice", "2|bob", "3|carol"]);
    assert!(after_reading_it.pasted(rows).is_none());
    // Pasting that read back, numbers and all, is still caught.
    assert_eq!(
        after_reading_it.pasted("1|1|alice\n2|2|bob\n").as_deref(),
        Some("1|alice\n2|bob\n")
    );
}

#[test]
fn only_recent_pages_are_remembered() {
    let pages = ShownPages::default();
    for page in 0..40u64 {
        pages.remember(page * 10 + 1, vec![format!("a{page}"), format!("b{page}")]);
    }
    assert!(
        pages.pasted("1|a0\n2|b0\n").is_none(),
        "the oldest page was let go"
    );
    assert!(pages.pasted("391|a39\n392|b39\n").is_some());
}
