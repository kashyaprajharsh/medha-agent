use super::*;

#[test]
fn syntax_results_distinguish_new_errors_existing_errors_and_unknown_deltas() {
    for path in ["data.json", "data.toml", "data.yaml"] {
        let valid = match path {
            "data.json" => "{}",
            "data.toml" => "x = 1",
            _ => "x: 1",
        };
        let mut report = json!({});
        annotate(&mut report, path, valid, "{[");
        assert_eq!(
            report["syntax"]["introduced"].as_array().unwrap().len(),
            1,
            "{path}: {report}"
        );
        annotate(&mut report, path, "{[", "{[");
        assert_eq!(report["syntax"]["introduced"], json!([]));
        annotate(&mut report, path, "{[", valid);
        assert_eq!(report["syntax"]["resolved"].as_array().unwrap().len(), 1);
        assert_eq!(report["verified"], true);
    }
}

#[test]
fn invalid_new_json_reports_a_new_error_without_an_invented_old_error() {
    let mut result = json!({"snapshot":null});
    annotate(&mut result, "new.json", "", "{[");
    assert_eq!(result["syntax"]["introduced"].as_array().unwrap().len(), 1);
    assert_eq!(result["syntax"]["resolved"], json!([]));
}
