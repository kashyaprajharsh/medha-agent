//! A Medha backend for a test that asks about folders, one request at a time.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

const ROLES: wire::Roles = wire::Roles {
    host: "backend",
    guest: "client",
};

pub struct Backend {
    child: Child,
    runtime: tokio::runtime::Runtime,
    reader: BufReader<Box<dyn AsyncRead + Unpin>>,
    writer: Box<dyn AsyncWrite + Unpin>,
    asked: u64,
}

impl Backend {
    pub fn start(home: &Path, env: &[(&str, String)]) -> Self {
        std::fs::create_dir_all(home).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_medha"))
            .arg("serve")
            .env("MEDHA_HOME", home)
            .env("MEDHA_CRED_STORE", "file")
            .env_remove("MEDHA_API_KEY")
            .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let read = |name: &str| std::fs::read_to_string(home.join("serve").join(name));
        let deadline = Instant::now() + Duration::from_secs(60);
        let (reader, writer) = runtime.block_on(async {
            loop {
                if let (Ok(address), Ok(token)) = (read("address"), read("token"))
                    && let Ok(stream) = wire::connect(&address).await
                {
                    let (read, write) = tokio::io::split(stream);
                    let mut reader: BufReader<Box<dyn AsyncRead + Unpin>> =
                        BufReader::new(Box::new(read));
                    let mut writer: Box<dyn AsyncWrite + Unpin> = Box::new(write);
                    wire::greet(&mut reader, &mut writer, &token, ROLES)
                        .await
                        .expect("the backend did not admit a client holding its token");
                    break (reader, writer);
                }
                assert!(Instant::now() < deadline, "the backend never listened");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        Self {
            child,
            runtime,
            reader,
            writer,
            asked: 0,
        }
    }

    /// The answer about `folder`, or the words the request was refused in.
    pub fn ask(&mut self, folder: &Path, method: &str, params: Value) -> Result<Value, String> {
        self.asked += 1;
        let id = self.asked;
        let request = json!({"id": id, "folder": folder, "method": method, "params": params});
        let (reader, writer) = (&mut self.reader, &mut self.writer);
        let reply = self.runtime.block_on(async {
            assert!(wire::write_frame(writer, &request).await);
            let waiting = async {
                loop {
                    let frame = wire::read_frame(reader).await.expect("the backend closed");
                    if frame["id"] == json!(id) {
                        break frame;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(60), waiting)
                .await
                .expect("the backend went quiet")
        });
        match reply["error"]["message"].as_str() {
            Some(error) => Err(error.to_string()),
            None => Ok(reply["result"].clone()),
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
