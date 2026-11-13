//! A server the person adds by address is a connector like any other on the Connectors page.
use super::*;

/// A remote server added by URL is listed as a custom connector; a catalog app's server is not listed twice; a local command is not a connector.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_added_by_address_is_listed_as_a_custom_connector() {
    let world = World::new();
    let backend = world.backend();
    let mut client = backend.connect().await;
    let folder = world.folder("custom");
    client.open(&folder).await;
    for args in [
        json!(["acme-docs", "--url", "https://mcp.acme.dev/mcp"]),
        json!(["linear", "--url", "https://mcp.linear.app/mcp"]),
        json!(["local-tool", "--", "npx", "-y", "local-tool"]),
    ] {
        client
            .about(
                &folder,
                json!({"method": "settings.mcp.save", "params": {"args": args}}),
            )
            .await
            .unwrap();
    }
    let listed = client
        .about(&folder, json!({"method": "extensions.connectors"}))
        .await
        .unwrap();
    let rows = listed["connectors"].as_array().unwrap();
    let custom: Vec<&Value> = rows
        .iter()
        .filter(|row| row["category"] == "custom")
        .collect();
    assert_eq!(custom.len(), 1, "{listed}");
    assert_eq!(custom[0]["server"], "acme-docs");
    assert_eq!(custom[0]["name"], "acme-docs");
    assert_eq!(custom[0]["url"], "https://mcp.acme.dev/mcp");
    assert_eq!(
        rows.iter().filter(|row| row["server"] == "linear").count(),
        1,
        "a catalog app was listed twice: {listed}"
    );
}
