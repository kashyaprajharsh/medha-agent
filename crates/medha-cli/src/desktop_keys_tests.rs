use super::*;

fn config() -> config::Config {
    toml::from_str(
        r#"
default_model = "cloud"

[models.cloud]
protocol = "open-ai-chat"
base_url = "https://api.example.com/v1"
model = "big"
auth = "bearer"

[models.local]
protocol = "open-ai-chat"
base_url = "http://127.0.0.1:8080/v1"
model = "small"
auth = "none"
"#,
    )
    .unwrap()
}

#[test]
fn only_models_that_need_a_key_are_listed_and_values_never_appear() {
    let listed = list(&config());
    let models = listed["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "https://api.example.com/v1");
    assert_eq!(models[0]["label"], "cloud");
    assert_eq!(listed["search"].as_array().unwrap().len(), 2);
    assert!(listed["models"][0].get("key").is_none());
}

#[test]
fn keys_are_only_set_for_what_is_configured() {
    let cfg = config();
    let refuse = |params: Value| set(&cfg, &params).unwrap_err().to_string();
    assert_eq!(
        refuse(json!({"group": "model", "id": "https://attacker.example/v1", "key": "k"})),
        "No saved model uses that endpoint"
    );
    assert_eq!(
        refuse(json!({"group": "search", "id": "duckduckgo", "key": "k"})),
        "That search provider has no key"
    );
    assert_eq!(
        refuse(json!({"group": "mcp", "id": "nope", "key": "k"})),
        "MCP server not found"
    );
    assert_eq!(
        refuse(json!({"group": "model", "id": "https://api.example.com/v1", "key": "  "})),
        "Paste the key first"
    );
    assert_eq!(
        refuse(json!({"group": "other", "id": "x", "key": "k"})),
        "Unknown key"
    );
}
