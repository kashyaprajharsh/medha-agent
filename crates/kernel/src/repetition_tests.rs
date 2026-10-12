use super::looping;

/// Honest writing is never taken for a loop, however alike its lines are;
/// a model stuck repeating itself is.
#[test]
fn only_a_real_loop_is_called_one() {
    // Rows that share a long start but are each different: a batch, not a loop.
    let rows: String = (0..60)
        .map(|n| {
            format!(
                "INSERT INTO users (id, name, email) VALUES ({n}, 'user{n}', 'u{n}@example.com');\n"
            )
        })
        .collect();
    assert!(!looping(&rows));
    let prose = "The parser reads one token at a time and keeps a small stack of open \
        blocks. When a block closes it is folded into its parent, and the parent's span \
        is widened to cover it. Errors are collected, not thrown, so one bad line does \
        not hide the next. "
        .to_owned()
        + &(0..12)
            .map(|n| format!("Step {n} checks a different rule about how blocks may nest. "))
            .collect::<String>();
    assert!(!looping(&prose));
    // Too short to judge, even when it is all one phrase.
    assert!(!looping(&"again and again ".repeat(10)));

    // One line over and over.
    assert!(looping(
        &"I will now continue with the next step of the task.\n".repeat(40)
    ));
    // One long line with no breaks, the same sentence end to end.
    assert!(looping(
        &"Let me verify the configuration file once more before proceeding. ".repeat(30)
    ));
    // A long unit of several lines repeated exactly, after an honest start.
    let unit = "First the build is checked for errors and warnings in every crate.\n\
        Then the tests are run, the slow ones last, and their output is read.\n\
        Then the change is described in a few plain sentences for the reader.\n\
        Finally the working tree is checked again so nothing is left behind.\n";
    let unit_loop = format!("Here is the plan for this change.\n{}", unit.repeat(6));
    assert!(looping(&unit_loop));
}
