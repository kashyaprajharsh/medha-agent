//! One shared generated-output catalogue, derived from verified immutable
//! events. File existence and assistant prose are never publication evidence.
use kernel::{Event, EventKind};
use protocol::{ArtifactPage, ArtifactRevision, ArtifactSnapshot};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Parser, Tag, TagEnd};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

const PAGE_BYTES: usize = wire::MAX_FRAME / 4;
const READ_CHUNK: usize = 1024 * 1024;
fn publishing_tool(tool: &str) -> bool {
    matches!(tool, "write" | "edit")
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn inline(event: &Event, text: &str) -> Vec<ArtifactRevision> {
    let mut records = Vec::new();
    let mut block = None;
    let mut source = String::new();
    let mut index = 0;
    for part in Parser::new(text) {
        match part {
            MarkdownEvent::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let language = info
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .next()
                    .unwrap_or_default();
                let kind = match language {
                    "mermaid" => "diagram",
                    "svg" => "drawing",
                    "html" => "page",
                    _ => "",
                };
                // Identity comes from the durable event and position alone.
                // Nothing the model writes can name or join an output.
                let id = format!("medha-output-{}-{index}", event.id);
                block = Some((kind.to_owned(), id, !language.is_empty()));
                source.clear();
            }
            MarkdownEvent::Text(text) if block.is_some() => source.push_str(&text),
            MarkdownEvent::End(TagEnd::CodeBlock) => {
                if let Some((kind, id, counted)) = block.take() {
                    let whole = kind != "page"
                        || source
                            .trim_start()
                            .to_ascii_lowercase()
                            .starts_with("<!doctype html")
                        || source
                            .trim_start()
                            .to_ascii_lowercase()
                            .starts_with("<html");
                    if !kind.is_empty() && whole {
                        records.push(ArtifactRevision {
                            artifact_id: format!("inline:{id}:{kind}"),
                            revision_id: format!("{}:code:{index}", event.id),
                            predecessor: None,
                            source_event: event.id.to_string(),
                            source_index: index,
                            tool_call: None,
                            ts: event.ts,
                            name: title(&kind, &source),
                            inline: true,
                            source_digest: Some({
                                use sha2::{Digest, Sha256};
                                format!("{:x}", Sha256::digest(source.as_bytes()))
                            }),
                            kind,
                            source: Some(source.clone()),
                            snapshot: None,
                            path: None,
                            screen: None,
                        });
                    }
                    index += usize::from(counted);
                }
            }
            _ => {}
        }
    }
    records
}

/// Files the reply named that its run changed, as the backend found them when
/// the reply was saved. They are shown as they are now; no copy was kept.
/// Their places follow the reply's own blocks, from `after`.
fn named(event: &Event, after: usize) -> Vec<ArtifactRevision> {
    let Some(outputs) = event.payload["outputs"].as_array() else {
        return Vec::new();
    };
    outputs
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            let path = output["path"].as_str()?;
            let kind = tools::outputs::kind(std::path::Path::new(path))?;
            Some(ArtifactRevision {
                artifact_id: format!("file:{path}"),
                revision_id: format!("{}:named:{index}", event.id),
                predecessor: None,
                source_event: event.id.to_string(),
                source_index: after + index,
                tool_call: None,
                ts: event.ts,
                kind: kind.into(),
                name: std::path::Path::new(path)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
                inline: false,
                source_digest: None,
                source: None,
                snapshot: None,
                path: Some(path.into()),
                screen: None,
            })
        })
        .collect()
}

fn title(kind: &str, source: &str) -> String {
    source
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("accTitle:")
                .or_else(|| line.trim().strip_prefix("title:"))
        })
        .or_else(|| {
            source
                .split_once("<title>")
                .and_then(|(_, rest)| rest.split_once("</title>").map(|(title, _)| title))
        })
        .map(|title| title.trim().chars().take(80).collect())
        .unwrap_or_else(|| {
            match kind {
                "diagram" => "Diagram",
                "drawing" => "Drawing",
                _ => "Page",
            }
            .into()
        })
}

fn screen_key(screen: &Value, id: &str) -> String {
    // A server name and checkpoint ID can be reused after reconfiguration.
    // Namespace document identity by the recorded connection definition.
    serde_json::to_string(&(
        &screen["server"],
        &screen["resource"],
        &screen["identity"],
        id,
    ))
    .expect("screen identity serializes")
}

pub(crate) fn catalogue(events: &[Event]) -> Vec<ArtifactRevision> {
    let mut catalogue = Catalogue::default();
    for event in events {
        catalogue.add(event);
    }
    catalogue.records
}

/// A chat's outputs, built one saved event at a time. Folding the whole chat
/// and following it as it grows give the same list.
#[derive(Default)]
pub(crate) struct Catalogue {
    records: Vec<ArtifactRevision>,
    heads: HashMap<String, (String, String)>,
    intents: HashMap<String, String>,
    flat_text: bool,
    // The output the person's latest message asked to change: artifact,
    // revision, kind. It holds until the reply makes one of that kind or the
    // person writes again, so a later unrelated output is never joined to it.
    about: Option<(String, String, String)>,
}

impl Catalogue {
    /// Takes in the next saved event and returns the outputs it added.
    pub(crate) fn add(&mut self, event: &Event) -> &[ArtifactRevision] {
        let Self {
            records,
            heads,
            intents,
            flat_text,
            about,
        } = self;
        let before = records.len();
        let mut next = Vec::new();
        match event.kind {
            // A retry repeats a message already counted; hook context and
            // agent reports share the channel but are not the person.
            EventKind::UserMessage
                if event.is_from_person() && event.payload.get("retry_of").is_none() =>
            {
                *about = event.payload.get("about").and_then(|about| {
                    let artifact = about["artifact_id"].as_str()?;
                    let revision = about["revision_id"].as_str()?;
                    records
                        .iter()
                        .find(|record: &&ArtifactRevision| {
                            record.inline
                                && record.artifact_id == artifact
                                && record.revision_id == revision
                        })
                        .map(|record| (artifact.into(), revision.into(), record.kind.clone()))
                });
            }
            EventKind::ModelText => {
                if let Some(text) = event.payload["text"].as_str() {
                    next = inline(event, text);
                }
                let after = next.last().map_or(0, |record| record.source_index + 1);
                next.extend(named(event, after));
                *flat_text = true;
            }
            EventKind::ModelMessage => {
                if !*flat_text
                    && let Ok(message) =
                        serde_json::from_value::<kernel::ModelMessage>(event.payload.clone())
                {
                    let text: String = message
                        .parts
                        .into_iter()
                        .filter_map(|part| match part {
                            kernel::ContentPart::Text(text) => Some(text.text),
                            _ => None,
                        })
                        .collect();
                    next = inline(event, &text);
                }
                *flat_text = false;
            }
            EventKind::ModelIntent => {
                if let (Some(id), Some(tool)) =
                    (event.payload["id"].as_str(), event.payload["tool"].as_str())
                {
                    intents.insert(id.to_owned(), tool.to_owned());
                }
            }
            EventKind::ToolObs if event.payload["status"] == "ok" => {
                let payload = &event.payload["payload"];
                let call = event.payload["intent_id"].as_str().unwrap_or_default();
                let tool = event.payload["tool"]
                    .as_str()
                    .or_else(|| intents.get(call).map(String::as_str));
                if tool.is_some_and(publishing_tool)
                    && let Some(snapshot) = written(payload)
                {
                    let path = std::path::Path::new(&snapshot.path);
                    next.push(ArtifactRevision {
                        artifact_id: format!("file:{}", snapshot.path),
                        revision_id: format!("{}:file", event.id),
                        predecessor: None,
                        source_event: event.id.to_string(),
                        source_index: 0,
                        tool_call: Some(call.into()),
                        ts: event.ts,
                        kind: snapshot.kind.clone(),
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        inline: false,
                        source_digest: None,
                        source: None,
                        snapshot: Some(snapshot),
                        path: None,
                        screen: None,
                    });
                }
                if let Some(screen) = event
                    .payload
                    .get("screen")
                    .filter(|screen| screen["result"]["isError"] != true)
                {
                    // Every screen stands alone unless its server names the
                    // document it belongs to; then later screens of that
                    // document become its next versions.
                    let artifact = screen["result"]["_meta"]["io.medha.artifact"]["id"]
                        .as_str()
                        .filter(|id| token(id))
                        .map_or_else(
                            || format!("screen:{}", event.id),
                            |id| format!("screen:{}", screen_key(screen, id)),
                        );
                    next.push(ArtifactRevision {
                        artifact_id: artifact,
                        revision_id: format!("{}:screen", event.id),
                        predecessor: None,
                        source_event: event.id.to_string(),
                        source_index: 0,
                        tool_call: Some(call.into()),
                        ts: event.ts,
                        kind: "app".into(),
                        name: screen["tool"].as_str().unwrap_or("App").into(),
                        inline: false,
                        source_digest: None,
                        source: None,
                        snapshot: None,
                        path: None,
                        screen: Some(screen.clone()),
                    });
                }
            }
            _ => {}
        }
        for mut record in next {
            if let Some((artifact, revision, _)) =
                about.take_if(|(_, _, kind)| record.inline && *kind == record.kind)
            {
                record.artifact_id = artifact;
                record.predecessor = Some(revision);
            }
            if let Some((head, kind)) = heads.get(&record.artifact_id) {
                if kind == &record.kind {
                    record.predecessor.get_or_insert_with(|| head.clone());
                } else {
                    record.artifact_id.push_str(&format!(":{}", record.kind));
                }
            }
            heads.insert(
                record.artifact_id.clone(),
                (record.revision_id.clone(), record.kind.clone()),
            );
            records.push(record);
        }
        &records[before..]
    }
}

/// Follows one chat's log as it grows, so each saved event is read once and the
/// outputs it adds can be sent on, instead of listing the whole chat again.
#[derive(Default)]
pub(crate) struct Following {
    session: Option<ulid::Ulid>,
    position: u64,
    /// History that was already there when following began; it is taken in
    /// but not sent, since a window lists it when it opens the chat.
    known: u64,
    catalogue: Catalogue,
}

impl Following {
    /// Called as a turn starts, with the chat's head at that moment.
    pub(crate) fn begin(&mut self, session: ulid::Ulid, head: u64) {
        if self.session != Some(session) {
            *self = Self {
                session: Some(session),
                known: head,
                ..Self::default()
            };
        }
    }

    /// The outputs added by events saved since the last call.
    pub(crate) async fn added(
        &mut self,
        log: &(impl kernel::EventLog + ?Sized),
    ) -> Result<Vec<ArtifactRevision>, String> {
        let Some(session) = self.session else {
            return Ok(Vec::new());
        };
        let mut added = Vec::new();
        loop {
            let (_, next) = log
                .checked_history_record(session, self.position, None, false)
                .await
                .map_err(|error| error.to_string())?;
            let Some((position, event)) = next else {
                return Ok(added);
            };
            self.position = position;
            let records = self.catalogue.add(&event);
            if position > self.known {
                added.extend_from_slice(records);
            }
        }
    }
}

/// How the model is told which output a message is about: in words, by kind
/// and title, or by place when titles repeat. `None` unless the reference is an
/// inline output revision of this conversation, so a stale or forged one is
/// dropped before it is recorded.
pub(crate) fn described(
    events: &[Event],
    about: &protocol::OutputReference,
) -> Option<(String, String)> {
    let records = catalogue(events);
    let inline: Vec<&ArtifactRevision> = records.iter().filter(|record| record.inline).collect();
    let target = inline.iter().find(|record| {
        record.artifact_id == about.artifact_id && record.revision_id == about.revision_id
    })?;
    let noun = match target.kind.as_str() {
        "diagram" => "diagram",
        "drawing" => "drawing",
        _ => "page",
    };
    let mut outputs: Vec<&str> = Vec::new();
    let mut namesakes: Vec<&str> = Vec::new();
    for record in inline.iter().filter(|record| record.kind == target.kind) {
        if !outputs.contains(&record.artifact_id.as_str()) {
            outputs.push(&record.artifact_id);
        }
        if record.name == target.name && !namesakes.contains(&record.artifact_id.as_str()) {
            namesakes.push(&record.artifact_id);
        }
    }
    let versions: Vec<&&ArtifactRevision> = inline
        .iter()
        .filter(|record| record.artifact_id == target.artifact_id)
        .collect();
    let version = versions
        .iter()
        .position(|record| record.revision_id == target.revision_id)?;
    let which = if version + 1 < versions.len() {
        format!(", version {} of {}", version + 1, versions.len())
    } else {
        String::new()
    };
    let untitled = target.name == title(&target.kind, "");
    // The title is the model's own earlier words; keep it to one plain line.
    let name: String = target
        .name
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .collect();
    let lead = if outputs.len() == 1 {
        format!("About the {noun} you made earlier{which}:")
    } else if namesakes.len() == 1 && !untitled {
        format!("About the {noun} \"{name}\" you made earlier{which}:")
    } else {
        let place = outputs.iter().position(|id| *id == target.artifact_id)? + 1;
        format!(
            "About {noun} {place} of the {} you made in this chat{which}:",
            outputs.len()
        )
    };
    // What a surface shows beside the person's message in place of that sentence.
    let kind = title(&target.kind, "");
    let label = if untitled {
        kind
    } else {
        format!("{kind}: {name}")
    };
    Some((lead, label))
}

pub(crate) fn page(records: Vec<ArtifactRevision>, cursor: usize) -> Result<ArtifactPage, String> {
    if cursor > records.len() {
        return Err("Invalid artifact cursor.".into());
    }
    let mut revisions = Vec::new();
    let mut bytes = 0;
    let mut omitted = 0;
    let mut next = cursor;
    for mut record in records.into_iter().skip(cursor) {
        // List metadata only. Exact inline bytes are loaded through the same
        // checked, chunked revision reader as files, never duplicated in pages.
        if record.inline
            && record
                .source
                .as_ref()
                .is_some_and(|text| text.len() > 40 * 1024)
        {
            record.source = None;
        }
        if let Some(screen) = record.screen.as_ref() {
            let bytes = serde_json::to_vec(screen).map_err(|e| e.to_string())?;
            if bytes.len() > 40 * 1024 {
                use sha2::{Digest, Sha256};
                record.snapshot = Some(ArtifactSnapshot {
                    path: "Drawing.json".into(),
                    kind: "app".into(),
                    hash: format!("{:x}", Sha256::digest(&bytes)),
                    size: bytes.len(),
                    assets: Vec::new(),
                    missing: Vec::new(),
                });
                record.screen = None;
            }
        }
        let size = serde_json::to_vec(&record)
            .map_err(|e| e.to_string())?
            .len();
        if size > PAGE_BYTES / 2 {
            omitted += 1;
            next += 1;
            continue;
        }
        if bytes + size > PAGE_BYTES || revisions.len() >= 200 {
            return Ok(ArtifactPage {
                revisions,
                next_cursor: Some(next),
                omitted,
            });
        }
        bytes += size;
        next += 1;
        revisions.push(record);
    }
    Ok(ArtifactPage {
        revisions,
        next_cursor: None,
        omitted,
    })
}

pub(crate) async fn file(
    log: &impl kernel::EventLog,
    session: &str,
    revision: &str,
) -> Result<ArtifactSnapshot, String> {
    let session = ulid::Ulid::from_string(session).map_err(|_| "Invalid conversation.")?;
    let id = revision
        .split(':')
        .next()
        .and_then(|id| ulid::Ulid::from_string(id).ok())
        .ok_or("Invalid file revision.")?;
    let event = revision_event(log, session, id).await?;
    if revision == format!("{id}:file") {
        if event.kind != EventKind::ToolObs || event.payload["status"] != "ok" {
            return Err("This event is not a published file.".into());
        }
        if !event.payload["tool"].as_str().is_some_and(publishing_tool) {
            return Err("This event is not a built-in file publication.".into());
        }
        return written(&event.payload["payload"]).ok_or("This revision has no saved file.".into());
    }
    let (kind, bytes) = inline_bytes(&event, revision).ok_or("Invalid file revision.")?;
    use sha2::{Digest, Sha256};
    let extension = match kind.as_str() {
        "diagram" => "mmd",
        "drawing" => "svg",
        "app" => "json",
        _ => "html",
    };
    Ok(ArtifactSnapshot {
        path: format!("{}.{}", title(&kind, &bytes), extension),
        kind,
        hash: format!("{:x}", Sha256::digest(bytes.as_bytes())),
        size: bytes.len(),
        assets: Vec::new(),
        missing: Vec::new(),
    })
}

/// Older observations predate the trusted outer tool tag. Resolve only their
/// verified model intent, never a tool-controlled field inside its result.
async fn revision_event(
    log: &impl kernel::EventLog,
    session: ulid::Ulid,
    id: ulid::Ulid,
) -> Result<Event, String> {
    let mut event = log
        .checked_event(session, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("This revision does not exist in this chat.")?;
    if event.kind == EventKind::ToolObs && event.payload["tool"].is_null() {
        let call = event.payload["intent_id"].as_str().unwrap_or_default();
        let events = log
            .output_events(session)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(tool) = events
            .iter()
            .find(|intent| intent.kind == EventKind::ModelIntent && intent.payload["id"] == call)
            .and_then(|intent| intent.payload["tool"].as_str())
            .filter(|tool| publishing_tool(tool))
        {
            event.payload["tool"] = json!(tool);
        }
    }
    Ok(event)
}

/// A written file as it was saved, from the write's own recorded result.
/// Its text is that result's `new`; a page's assets are listed beside it and
/// read from its folder. Revisions written while copies were kept still name
/// theirs, and are read from the store.
fn written(payload: &Value) -> Option<ArtifactSnapshot> {
    let output = payload.get(tools::outputs::FIELD);
    if let Some(kept) = output.filter(|output| output["hash"].is_string()) {
        return serde_json::from_value(kept.clone()).ok();
    }
    let text = payload["new"].as_str()?;
    let path = output
        .and_then(|output| output["path"].as_str())
        .or_else(|| payload["path"].as_str())?;
    let kind = tools::outputs::kind(std::path::Path::new(path))?;
    fn listed<T: serde::de::DeserializeOwned>(output: Option<&Value>, field: &str) -> Vec<T> {
        output
            .and_then(|output| serde_json::from_value(output[field].clone()).ok())
            .unwrap_or_default()
    }
    use sha2::{Digest, Sha256};
    Some(ArtifactSnapshot {
        path: path.into(),
        kind: kind.into(),
        hash: format!("{:x}", Sha256::digest(text.as_bytes())),
        size: text.len(),
        assets: listed(output, "assets"),
        missing: match output {
            Some(_) => listed(output, "missing"),
            None if kind == "page" => {
                vec!["Local dependencies were not listed for this older revision.".into()]
            }
            None => Vec::new(),
        },
    })
}

fn inline_bytes(event: &Event, revision: &str) -> Option<(String, String)> {
    if revision == format!("{}:file", event.id)
        && event.kind == EventKind::ToolObs
        && event.payload["status"] == "ok"
        && event.payload["tool"]
            .as_str()
            .is_some_and(|tool| matches!(tool, "write" | "edit"))
        && !event.payload["payload"][tools::outputs::FIELD]["hash"].is_string()
    {
        let payload = &event.payload["payload"];
        let kind = tools::outputs::kind(std::path::Path::new(payload["path"].as_str()?))?;
        return Some((kind.into(), payload["new"].as_str()?.into()));
    }
    if revision == format!("{}:screen", event.id) {
        let screen = (event.kind == EventKind::ToolObs && event.payload["status"] == "ok")
            .then(|| event.payload.get("screen"))
            .flatten()?;
        if screen["result"]["isError"] == true {
            return None;
        }
        return Some(("app".into(), serde_json::to_string(screen).ok()?));
    }
    let text = match event.kind {
        EventKind::ModelText => event.payload["text"].as_str()?.to_owned(),
        EventKind::ModelMessage => {
            serde_json::from_value::<kernel::ModelMessage>(event.payload.clone())
                .ok()?
                .parts
                .into_iter()
                .filter_map(|part| match part {
                    kernel::ContentPart::Text(text) => Some(text.text),
                    _ => None,
                })
                .collect()
        }
        _ => return None,
    };
    let record = inline(event, &text)
        .into_iter()
        .find(|record| record.revision_id == revision)?;
    Some((record.kind, record.source?))
}

pub(crate) async fn read(
    log: &impl kernel::EventLog,
    store: &Arc<dyn kernel::ArtifactStore>,
    request: protocol::ReadArtifact,
) -> Result<Value, String> {
    use base64::Engine;
    let snapshot = file(log, &request.session_id, &request.revision_id).await?;
    let (hash, size) = if let Some(ref path) = request.asset {
        let asset = snapshot
            .assets
            .iter()
            .find(|asset| &asset.path == path)
            .ok_or("This asset is not in the revision's manifest.")?;
        let hash = asset
            .hash
            .as_ref()
            .ok_or("This asset was not kept; it is read from the document's folder.")?;
        (hash, asset.size)
    } else {
        (&snapshot.hash, snapshot.size)
    };
    if size > 24 * 1024 * 1024 || request.offset > size {
        return Err("Invalid artifact size or range.".into());
    }
    let event = revision_event(
        log,
        ulid::Ulid::from_string(&request.session_id).map_err(|_| "Invalid conversation.")?,
        ulid::Ulid::from_string(request.revision_id.split(':').next().unwrap_or_default())
            .map_err(|_| "Invalid revision.")?,
    )
    .await?;
    let count = READ_CHUNK.min(size - request.offset);
    let data = if request.asset.is_none()
        && let Some((_, text)) = inline_bytes(&event, &request.revision_id)
    {
        text.as_bytes()[request.offset..request.offset + count].to_vec()
    } else {
        Arc::clone(store)
            .get_async(hash.clone(), request.offset, Some(count))
            .await?
    };
    if data.is_empty() && request.offset < size {
        return Err("The stored artifact is truncated.".into());
    }
    Ok(json!(protocol::ArtifactBytes {
        data: base64::engine::general_purpose::STANDARD.encode(data),
        size
    }))
}

#[cfg(test)]
#[path = "application_artifacts_tests.rs"]
mod tests;
