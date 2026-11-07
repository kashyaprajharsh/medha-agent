//! An MCP server's images reach the model sized for the conversation, a few at a time.

use kernel::{Executor, MediaSource, ToolIntent};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tools::ToolRegistry;

const SERVER: &str = r#"
import sys, json, base64, glob, os
folder = sys.argv[1]
def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    msg = json.loads(line); mid = msg.get("id"); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":msg["params"].get("protocolVersion","2025-06-18"),
              "capabilities":{"tools":{}},"serverInfo":{"name":"frames","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"frames","description":"d",
              "inputSchema":{"type":"object","properties":{}}}]}})
    elif method == "tools/call":
        frames = sorted(glob.glob(os.path.join(folder, "frame*.png")))
        content = [{"type":"text","text":"five frames"}] + [
            {"type":"image","mimeType":"image/png","data":base64.b64encode(open(f,"rb").read()).decode()}
            for f in frames]
        send({"jsonrpc":"2.0","id":mid,"result":{"content":content,"isError":False}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;

#[derive(Default)]
struct Stored(Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl kernel::ArtifactStore for Stored {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let mut map = self.0.lock().unwrap();
        let hash = format!("h{}", map.len());
        map.insert(hash.clone(), bytes.to_vec());
        Ok(hash)
    }
    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let map = self.0.lock().unwrap();
        let data = map.get(hash).ok_or("not found")?;
        let end = len.map_or(data.len(), |len| (offset + len).min(data.len()));
        Ok(data[offset.min(end)..end].to_vec())
    }
    fn size(&self, hash: &str) -> Result<usize, String> {
        Ok(self.0.lock().unwrap().get(hash).map_or(0, Vec::len))
    }
}

/// A full-HD frame with fine detail, so it cannot pass at full size by compressing well.
fn frame(seed: u32) -> Vec<u8> {
    let (width, height) = (1920u32, 1080u32);
    let mut state = seed;
    let pixels: Vec<u8> = (0..width * height * 4)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    media::from_rgba(width, height, &pixels).unwrap()
}

/// An MCP tool's images reach the model, four at most and each small; the rest are kept and named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_tools_images_reach_the_model_sized_and_the_rest_are_kept() {
    if !sandbox::program_on_path("python3") {
        eprintln!("skipping: python3 is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.py"), SERVER).unwrap();
    for index in 0..5 {
        std::fs::write(dir.path().join(format!("frame{index}.png")), frame(index)).unwrap();
    }
    let command = vec![
        "python3".to_string(),
        dir.path().join("server.py").to_string_lossy().into_owned(),
        dir.path().to_string_lossy().into_owned(),
    ];
    let manager = Arc::new(mcp::McpManager::new(
        dir.path().to_path_buf(),
        mcp::Config {
            enabled: true,
            servers: vec![mcp::ServerConfig {
                id: "cam".into(),
                transport: mcp::Transport::Stdio {
                    command,
                    env: Vec::new(),
                },
                ..Default::default()
            }],
            ..mcp::Config::default()
        },
    ));
    manager.connect_startup().await;
    let stored = Arc::new(Stored::default());
    let sandbox = Arc::new(sandbox::WorkspaceSandbox::new_jailed(dir.path()).unwrap());
    let mut registry = ToolRegistry::with_workspace(sandbox, stored.clone());
    registry.register_mcp(Arc::clone(&manager));

    let intent = ToolIntent {
        id: "call-1".into(),
        tool: "mcp__cam__frames".into(),
        args: json!({}),
    };
    let obs = registry.execute(&intent).await;
    manager.shutdown().await;

    assert_eq!(
        obs.media.len(),
        4,
        "images sent to the model: {}",
        obs.payload
    );
    for part in &obs.media {
        let MediaSource::Artifact(hash) = &part.source else {
            panic!("an image must be stored, not inlined")
        };
        let kept = kernel::ArtifactStore::get(stored.as_ref(), hash, 0, None).unwrap();
        assert!(part.width.unwrap().max(part.height.unwrap()) <= 1568);
        assert!(
            kept.len() <= 256 * 1024,
            "an image kept at {} bytes",
            kept.len()
        );
    }
    let said = obs.payload.to_string();
    assert!(
        said.contains("1920×1080"),
        "original sizes are not recorded: {said}"
    );
    assert!(
        said.contains("read"),
        "the fifth image is not named for reading: {said}"
    );
}
