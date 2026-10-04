//! One connected client: its requests to the backend, and its requests to the
//! chats it has attached to. It may only speak to a chat it is attached to.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::KEPT_FRAMES;
use crate::{Backend, Chats, Failure, PROTOCOL, refused};

/// Room for a full replay and then some; a client further behind than this is dropped.
const QUEUE: usize = KEPT_FRAMES + 1024;

#[derive(Clone)]
pub(crate) struct Client {
    pub(crate) id: u64,
    out: mpsc::Sender<Arc<str>>,
    dropped: CancellationToken,
}

impl Client {
    /// Never waits: a client that cannot keep up is disconnected, and may come back with its cursor.
    pub(crate) fn send(&self, line: Arc<str>) -> bool {
        let sent = self.out.try_send(line).is_ok();
        if !sent {
            self.dropped.cancel();
        }
        sent
    }

    fn reply(&self, id: Option<Value>, outcome: Result<Value, Failure>) {
        let Some(id) = id else { return };
        let frame = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        let mut line = frame.to_string();
        line.push('\n');
        self.send(line.into());
    }
}

pub(crate) async fn serve<C, R, W>(backend: &Arc<Backend<C>>, reader: R, mut writer: W)
where
    C: Chats,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut queued) = mpsc::channel::<Arc<str>>(QUEUE);
    let me = Client {
        id: backend.clients.fetch_add(1, Ordering::Relaxed),
        out,
        dropped: CancellationToken::new(),
    };
    let writing = tokio::spawn({
        let dropped = me.dropped.clone();
        async move {
            let writes = async {
                while let Some(line) = queued.recv().await {
                    if writer.write_all(line.as_bytes()).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        break;
                    }
                }
            };
            // A write to a client that stopped reading must not outlive its being dropped.
            tokio::select! {
                () = writes => {}
                () = dropped.cancelled() => {}
            }
            dropped.cancel();
        }
    });
    let mut reader = BufReader::new(reader);
    let mut attached = HashSet::new();
    loop {
        let frame = tokio::select! {
            () = me.dropped.cancelled() => break,
            frame = wire::read_frame(&mut reader) => frame,
        };
        let Some(frame) = frame else { break };
        handle(backend, &me, &mut attached, frame).await;
    }
    for id in attached {
        if let Ok(session) = backend.find(&id) {
            session.detach(me.id);
        }
    }
    // Chats may still hold this client for answers it will never read.
    me.dropped.cancel();
    let _ = writing.await;
}

async fn handle<C: Chats>(
    backend: &Arc<Backend<C>>,
    me: &Client,
    attached: &mut HashSet<String>,
    mut frame: Value,
) {
    let id = frame.get("id").cloned();
    let Some(method) = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let named = frame
        .as_object_mut()
        .and_then(|frame| frame.remove("session"))
        .and_then(|session| session.as_str().map(str::to_owned));
    let session = |needed: &str| {
        let id = named
            .as_deref()
            .ok_or_else(|| (-32602, format!("{needed} needs a session")))?;
        backend.find(id)
    };
    // These can take long, so the client's other requests are not held behind them.
    let about_folder = named.is_none() && frame.get("folder").is_some();
    if method == "session.create" || about_folder {
        let (backend, me) = (Arc::clone(backend), me.clone());
        tokio::spawn(async move {
            let outcome = match about_folder {
                true => backend.chats.about_folder(&frame).await.map_err(refused),
                false => backend.create(&frame["params"]).await,
            };
            me.reply(id, outcome);
        });
        return;
    }
    // A request that names a chat is the chat's, whatever it is called, unless it is about attaching.
    let outcome = match method.as_str() {
        "hello" if named.is_none() => {
            Ok(json!({ "backend": backend.version, "protocol": PROTOCOL }))
        }
        "session.list" if named.is_none() => Ok(backend.list()),
        "session.attach" => session("session.attach").map(|session| {
            attached.insert(session.id.clone());
            session.attach(me, frame["params"]["after"].as_u64())
        }),
        "session.detach" => session("session.detach").map(|session| {
            attached.remove(&session.id);
            session.detach(me.id);
            json!({ "detached": true })
        }),
        "session.close" => match session("session.close") {
            Ok(session) if attached.contains(&session.id) => {
                session.close().await;
                Ok(json!({ "closing": true }))
            }
            Ok(_) => Err(refused("attach to the session first")),
            Err(error) => Err(error),
        },
        _ => match session("a request to a chat") {
            Ok(session) if attached.contains(&session.id) => {
                match session.forward(me, frame).await {
                    // The chat answers; its answer is routed back to this client.
                    Ok(()) => return,
                    Err(error) => Err(refused(error)),
                }
            }
            Ok(_) => Err(refused("attach to the session first")),
            Err(error) => Err(error),
        },
    };
    me.reply(id, outcome);
}
