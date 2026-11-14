use crate::{Shell, read_as};

fn words(line: &str) -> Option<Vec<Vec<String>>> {
    let reading = read_as(line, Shell::PowerShell);
    reading.complete.then(|| {
        reading
            .commands
            .iter()
            .map(|command| command.words.iter().map(|word| word.text.clone()).collect())
            .collect()
    })
}

/// Every line in the table reads to exactly its commands, or is refused when it cannot be read for certain.
#[test]
fn each_powershell_line_reads_to_its_commands_or_is_refused() {
    let table: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/powershell_reading.json")).unwrap();
    assert_eq!(table.len(), 68);
    for case in table {
        let script = case["script"].as_str().unwrap();
        let expected: Option<Vec<Vec<String>>> =
            serde_json::from_value(case["expected"].clone()).unwrap();
        assert_eq!(words(script), expected, "{}: {script}", case["name"]);
    }
}

/// Output sent to $null, merged streams and Out-Null write nothing; any other redirect still cannot be read.
#[test]
fn discarded_output_is_no_write_and_quoted_text_is_left_alone() {
    let reads = |line: &str| read_as(line, Shell::PowerShell).reads_only();
    assert!(reads(
        "Get-ChildItem -Recurse -Filter *.ts | Select-Object -First 20"
    ));
    assert!(reads("Get-Content src\\main.rs -TotalCount 40"));
    assert!(reads(
        "Select-String -Path README.md -Pattern 'TODO' 2>$null"
    ));
    assert!(reads("Test-Path dist > $null; Get-Location"));
    assert!(reads("git status 2>&1 | Out-String"));
    assert!(!reads("New-Item -ItemType Directory out | Out-Null"));
    assert!(!reads("Remove-Item build -Recurse -Force"));
    assert!(!reads("npm test"));
    // A cmdlet's name means nothing to bash: there `compare` is a program that writes its third file.
    assert!(!reads("Compare-Object (Get-Content a) b"));
    assert!(reads("compare a.txt b.txt"));
    assert!(!crate::read("compare a.png b.png diff.png").reads_only());
    assert_eq!(words("Get-Content notes.txt > out.txt"), None);
    assert_eq!(
        words("if (Test-Path dist) { Remove-Item dist -Recurse }"),
        None
    );
    assert_eq!(
        words("Write-Output 'kept >$null' 2>$null"),
        Some(vec![vec!["Write-Output".into(), "kept >$null".into()]])
    );
}
