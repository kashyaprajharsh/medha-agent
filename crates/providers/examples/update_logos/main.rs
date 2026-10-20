//! Refresh the desktop's provider logos from models.dev; run by hand after the catalogue update:
//! `cargo run -p providers --example update_logos`

mod rules;

use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

const URL: &str = "https://models.dev/logos/";
/// Medha's own local entries whose mark models.dev keeps under another id.
const SAME_MARK: &[(&str, &str)] = &[("ollama", "ollama-cloud"), ("lm-studio", "lmstudio")];

type Error = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = root.join("crates/providers/src/fixtures/provider-catalogue.json");
    let catalogue: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(fixture)?)?;
    let mut ids: Vec<&str> = catalogue.keys().map(String::as_str).collect();
    ids.extend(SAME_MARK.iter().map(|(id, _)| *id));
    ids.sort_unstable();
    ids.dedup();

    let client = reqwest::Client::builder()
        .user_agent("medha-logo-update")
        .timeout(Duration::from_secs(15))
        .build()?;
    // Whatever models.dev answers for an id that cannot exist is its placeholder, never a mark.
    let placeholder = Sha256::digest(fetch(&client, "medha-no-such-provider").await?);
    let mut logos = Vec::new();
    for chunk in ids.chunks(8) {
        let marks = chunk.iter().map(|id| {
            let mark = SAME_MARK
                .iter()
                .find(|(own, _)| own == id)
                .map_or(*id, |(_, mark)| *mark);
            fetch(&client, mark)
        });
        for (id, data) in chunk.iter().zip(futures::future::join_all(marks).await) {
            logos.push((*id, data?));
        }
    }

    // Everything is fetched and checked before the folder is replaced, so a failed run keeps the old logos.
    let out = root.join("apps/desktop/src/assets/providers");
    let staged = root.join("apps/desktop/src/assets/providers.new");
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::create_dir_all(&staged)?;
    let (mut kept, mut bytes, mut unmarked, mut refused) = (0, 0, 0, Vec::new());
    for (id, data) in logos {
        if Sha256::digest(&data) == placeholder {
            unmarked += 1;
        } else if let Some(why) = rules::refusal(&data) {
            refused.push(format!("{id} ({why})"));
        } else {
            std::fs::write(staged.join(format!("{id}.svg")), &data)?;
            kept += 1;
            bytes += data.len();
        }
    }
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::rename(&staged, &out)?;
    println!("{kept} logos, {bytes} bytes; {unmarked} with no mark");
    println!(
        "refused: {}",
        if refused.is_empty() {
            "none".into()
        } else {
            refused.join(", ")
        }
    );
    println!("placeholder sha256 {placeholder:x}");
    Ok(())
}

/// One retry, since a busy server answers the odd request with a 502.
async fn fetch(client: &reqwest::Client, id: &str) -> Result<Vec<u8>, Error> {
    let url = format!("{URL}{id}.svg");
    let mut last = None;
    for _ in 0..2 {
        match client
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(response) => return Ok(response.bytes().await?.to_vec()),
            Err(error) => last = Some(error),
        }
    }
    Err(format!("{url}: {}", last.expect("tried")).into())
}
