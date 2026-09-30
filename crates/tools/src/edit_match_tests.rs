use super::*;

fn applied(content: &str, old: &str, new: &str) -> Replaced {
    replace(content, old, new, false).unwrap_or_else(|miss| {
        let why = match miss {
            Miss::NotFound => "not found".to_string(),
            Miss::Ambiguous(n) => format!("{n} matches"),
        };
        panic!("expected a match, got {why}")
    })
}

/// A copy that no longer describes the file must fail, not overwrite the
/// file with the model's memory of it.
#[test]
fn a_stale_copy_never_rewrites_the_text_it_misremembers() {
    let stale = [
        (
            "def check(x):\n    if x >= 0:\n        return 1\n    return 0\n",
            "def check(x):\n    if x > 0:\n        return 1\n    return 0\n",
            "def check(x):\n    if x > 0:\n        return 2\n    return 0\n",
        ),
        (
            "limit = 100\nif used > limit:\n    deny()\nlog()\n",
            "limit = 10\nif used > limit:\n    deny()\nlog()\n",
            "limit = 10\nif used > limit:\n    deny()\n    alert()\nlog()\n",
        ),
        ("total = a  + b\n", "total = a + b\n", "total = a + c\n"),
    ];
    for (file, old, new) in stale {
        assert!(
            matches!(replace(file, old, new, false), Err(Miss::NotFound)),
            "stale copy matched: {old:?}"
        );
    }
}

/// A block copied at the wrong depth lands at the file's depth, and the lines
/// the model did not change stay byte-identical.
#[test]
fn an_indentation_shift_lands_at_the_files_depth() {
    let file = "class A:\n    def f(self):\n        x = 1\n        return x\n";
    let old = "def f(self):\n    x = 1\n    return x\n";
    let new = "def f(self):\n    x = 2\n    if x:\n        return x\n";
    let done = applied(file, old, new);
    assert!(done.loose);
    assert_eq!(
        done.text,
        "class A:\n    def f(self):\n        x = 2\n        if x:\n            return x\n"
    );

    let tabs = "fn main() {\n\tlet a = 1;\n\tlet b = 2;\n}\n";
    let done = applied(tabs, "let a = 1;  \nlet b = 2;", "let a = 10;\nlet b = 2;");
    assert_eq!(done.text, "fn main() {\n\tlet a = 10;\n\tlet b = 2;\n}\n");
}

/// Ignoring indentation must not ignore nesting: in Python the two blocks are
/// different programs.
#[test]
fn a_copy_with_different_nesting_is_not_the_same_block() {
    let file = "if a:\n    b()\n    c()\n";
    let flattened = "if a:\n    b()\nc()\n";
    assert!(matches!(
        replace(file, flattened, "if a:\n    b()\nd()\n", false),
        Err(Miss::NotFound)
    ));
    let mixed = "if a:\n\tb()\n";
    assert!(matches!(
        replace(mixed, "if a:\n    b()\n", "if a:\n    z()\n", false),
        Err(Miss::NotFound)
    ));
}

/// Plain quotes and dashes in the copy still find the file's typographic ones,
/// and the file keeps its own characters wherever the model changed nothing.
#[test]
fn typographic_drift_matches_and_keeps_the_files_characters() {
    let file = "say(\u{201C}it\u{2019}s done\u{201D}) \u{2014} ok\n";
    let done = applied(
        file,
        "say(\"it's done\") - ok",
        "say(\"it's finished\") - ok",
    );
    assert!(done.loose);
    assert_eq!(
        done.text,
        "say(\u{201C}it\u{2019}s finished\u{201D}) \u{2014} ok\n"
    );

    let lines = "  title: \u{201C}Draft\u{201D}\n  body: plain\n";
    let done = applied(
        lines,
        "title: \"Draft\"\nbody: plain\n",
        "title: \"Draft\"\nbody: rich\n",
    );
    assert_eq!(done.text, "  title: \u{201C}Draft\u{201D}\n  body: rich\n");
}

/// A loose match is held to the same uniqueness rule as an exact one.
#[test]
fn loose_matches_are_counted_before_anything_is_replaced() {
    let file = "fn a() {\n    go();\n}\nfn b() {\n        go();\n}\n";
    assert!(matches!(
        replace(file, "  go();  \n", "  stop();\n", false),
        Err(Miss::Ambiguous(2))
    ));
    let all = replace(file, "  go();  \n", "  stop();\n", true)
        .ok()
        .unwrap();
    assert!(all.loose);
    assert_eq!(all.count, 2);
    assert_eq!(
        all.text,
        "fn a() {\n    stop();\n}\nfn b() {\n        stop();\n}\n"
    );
}

#[test]
fn line_endings_and_blank_copies_are_handled_before_loose_matching() {
    let crlf = "a\r\n    b\r\nc\r\n";
    let done = applied(crlf, "b\nc\n", "B\nc\n");
    assert_eq!(done.text, "a\r\n    B\r\nc\r\n");
    let exact = applied(crlf, "a\n", "A\n");
    assert!(!exact.loose);
    assert_eq!(exact.text, "A\r\n    b\r\nc\r\n");

    assert!(matches!(
        replace("x\n\n  \ny\n", "   \t\n", "", false),
        Err(Miss::NotFound)
    ));
    let deleted = applied("keep\n    drop\nkeep too\n", "  drop  \n", "");
    assert!(deleted.loose);
    assert_eq!(deleted.text, "keep\nkeep too\n");
}

/// One place matching by typography and another by indentation is still two
/// candidates; picking either would be a guess.
#[test]
fn candidates_from_different_tolerances_are_counted_together() {
    let file = "say(\u{201C}hi\u{201D})\nend\n    say(\"hi\")\n    end\n";
    assert!(matches!(
        replace(file, "say(\"hi\")\nend", "say(\"yo\")\nend", false),
        Err(Miss::Ambiguous(2))
    ));
}

/// Loose matching never reaches inside a line or past the file's own shape.
#[test]
fn copies_it_cannot_place_safely_are_refused() {
    // Only part of a line, with drifted indentation.
    assert!(matches!(
        replace("    let a = f(x);\n", "  f(x)", "  g(x)", false),
        Err(Miss::NotFound)
    ));
    // Longer than the file.
    assert!(matches!(
        replace("a\n", "  a\n  b\n  c\n", "x\n", false),
        Err(Miss::NotFound)
    ));
    // Non-breaking spaces are not indentation.
    assert!(matches!(
        replace("\u{A0}\u{A0}x = 1\n", "x = 1 \n", "x = 2\n", false),
        Err(Miss::NotFound)
    ));
    // Tabs in the file, spaces in the copy: the depth cannot be known.
    assert!(matches!(
        replace(
            "\tif a {\n\t\tb();\n\t}\n",
            "    if a {\n        b();\n    }\n",
            "x\n",
            false
        ),
        Err(Miss::NotFound)
    ));
}

/// A copy deeper than the file shifts left, and a new line shallower than the
/// copy's base clamps to the margin rather than borrowing indentation.
#[test]
fn a_leftward_shift_clamps_at_the_margin() {
    let file = "    x = 1\n    y = 2\n";
    let old = "        x = 1\n        y = 2\n";
    let new = "        x = 1\n    z = 0\n  w = 0\n        y = 2\n";
    assert_eq!(
        applied(file, old, new).text,
        "    x = 1\nz = 0\nw = 0\n    y = 2\n"
    );
}

/// The file's end, its multibyte text and its repeated blocks survive intact.
#[test]
fn file_edges_are_preserved() {
    let unterminated = applied("a\n    b", "b  \n", "c\n");
    assert_eq!(unterminated.text, "a\n    c");

    let wide = "\u{1F600} \u{201C}\u{00E9}t\u{00E9}\u{201D} \u{1F600}\n";
    let done = applied(
        wide,
        "\u{1F600} \"\u{00E9}t\u{00E9}\" \u{1F600}",
        "\u{1F600} \"hiver\" \u{1F600}",
    );
    assert_eq!(done.text, "\u{1F600} \u{201C}hiver\u{201D} \u{1F600}\n");

    let repeated = replace("  a\n  a\n  a\n", "a \na \n", "b\nb\n", true)
        .ok()
        .unwrap();
    assert_eq!(repeated.count, 1, "windows never overlap");
    assert_eq!(repeated.text, "  b\n  b\n  a\n");
}

/// Blank lines in the copy match blank lines only, and the file keeps its own.
#[test]
fn blank_lines_do_not_stand_in_for_content() {
    let file = "fn a() {\n    one();\n  \n    two();\n}\n";
    let done = applied(file, "one();\n\ntwo();\n", "one();\n\nthree();\n");
    assert_eq!(done.text, "fn a() {\n    one();\n  \n    three();\n}\n");
    assert!(matches!(
        replace("one();\ntwo();\n", "one();\n\ntwo();\n", "x\n", false),
        Err(Miss::NotFound)
    ));
}
