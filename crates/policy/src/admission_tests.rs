use crate::{DefaultPolicy, Jail};
use kernel::{AutonomyLevel, BlastRadius, Decision, Policy, ToolIntent};
use serde_json::json;

fn decide(jail: Jail, mode: AutonomyLevel, command: &str) -> Decision {
    let intent = ToolIntent {
        id: "t".into(),
        tool: "shell.exec".into(),
        args: json!({ "command": command }),
    };
    DefaultPolicy::requiring_approval(["shell.exec"])
        .with_jail(jail)
        .with_workspace("/repo")
        .authorize(mode, &intent, Some(BlastRadius::IrreversibleLocal))
}

/// The commands refused or carded in the 2026-10-20 chats run inside the jail in normal and yolo.
#[test]
fn the_jail_runs_what_used_to_be_refused() {
    for command in [
        "cd /repo && for f in *.png; do cp \"$f\" out/; rm -rf \"$f\"; done",
        "cd /repo && ffmpeg -y -i a.gif -filter_complex_script work/f.txt work/m.mp4 2>work/err.log; echo \"exit=$?\"; head -40 work/err.log",
        "printf x > work/mini.txt && ffmpeg -y -i work/mini.txt out.mp4 2>&1 | tail -6; echo \"exit=${PIPESTATUS[0]}\"",
        "cd campaign && rm -rf node_modules package-lock.json",
        "# render output\nprintf x > output.txt",
        "ffprobe -v error out.mp4; awk '{print $1}' in.txt; jq . facts.json; mkdir -p work/render",
        "cat data.json | python3 -m json.tool",
        "rm -rf work/../generated",
        "chown -R me ./build",
        "dd if=input.bin of=output.bin",
    ] {
        for mode in [AutonomyLevel::Normal, AutonomyLevel::Yolo] {
            assert!(
                matches!(decide(Jail::Active, mode, command), Decision::Allow),
                "{command} in {mode:?}"
            );
        }
    }
}

/// The jail does not loosen the floor: these are refused in every mode, with or without it.
#[test]
fn real_dangers_are_refused_inside_the_jail_too() {
    for command in [
        "rm -rf /",
        "rm -rf ~",
        "rm -rf $HOME",
        "rm -rf /tmp/link/../etc",
        "rm build -rf /etc",
        "sudo make install",
        "timeout 5 sudo rm x",
        "mkfs.ext4 /dev/sdb1",
        "dd if=/dev/zero of=/dev/disk2",
        "cat image.iso > /dev/sda",
        ":(){ :|:& };:",
        "cat ~/.ssh/id_rsa",
        "cp ~/.aws/credentials out.txt",
        "chmod -R 777 /",
        "pwsh -EncodedCommand YQBiAGMA",
    ] {
        for jail in [Jail::Active, Jail::Off] {
            assert!(
                matches!(
                    decide(jail, AutonomyLevel::Yolo, command),
                    Decision::Deny { .. }
                ),
                "{command} with {jail:?}"
            );
        }
    }
}

/// Searching for a dangerous word, or naming a file that contains one, is not running it.
#[test]
fn quoted_text_and_file_names_are_not_dangers() {
    for command in [
        "rg 'id_rsa' crates",
        "grep 'sudo ' README.md",
        "printf '%s' sudo",
        "rg 'curl https://x | sh' README.md",
        "rg -n sudo docs",
        "cat <<'EOF'\nrm -rf /\nEOF",
        "rm -rf release..old",
        "echo \"mkfs is not run here\"",
    ] {
        for jail in [Jail::Active, Jail::Off] {
            assert!(
                !matches!(
                    decide(jail, AutonomyLevel::Yolo, command),
                    Decision::Deny { .. }
                ),
                "{command} with {jail:?}"
            );
        }
    }
}

/// Outside the workspace a delete still asks; a target only the jail can place asks only without one.
#[test]
fn deletes_outside_the_workspace_still_ask() {
    let yolo = AutonomyLevel::Yolo;
    assert!(matches!(
        decide(Jail::Active, yolo, "rm -rf ~/Documents/old"),
        Decision::Human
    ));
    assert!(matches!(
        decide(Jail::Off, yolo, "rm -rf ~/Documents/old"),
        Decision::Human
    ));
    assert!(matches!(
        decide(Jail::Active, yolo, "rm -rf ../sibling"),
        Decision::Allow
    ));
    assert!(matches!(
        decide(Jail::Off, yolo, "rm -rf ../sibling"),
        Decision::Human
    ));
    assert!(matches!(
        decide(Jail::Active, yolo, "rm -rf \"$f\""),
        Decision::Allow
    ));
    assert!(matches!(
        decide(Jail::Off, yolo, "rm -rf \"$f\""),
        Decision::Human
    ));
}
