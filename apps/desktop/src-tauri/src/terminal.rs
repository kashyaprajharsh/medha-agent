//! The user's terminal, independent of Medha's agent executor.
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type OpenTerminals = Arc<Mutex<HashMap<String, Terminal>>>;
type Output = Box<dyn Read + Send>;

struct Terminal {
    master: Box<dyn MasterPty + Send>,
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    generation: Arc<()>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            let foreground = self.master.process_group_leader();
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            #[cfg(unix)]
            if let Some(group) = foreground.filter(|group| *group > 1) {
                // PTY foreground jobs may have their own process group.
                unsafe {
                    libc::kill(-group, libc::SIGHUP);
                }
            }
            // Signal before window destruction returns; reap off the UI thread.
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub struct Terminals {
    workspace: PathBuf,
    open: OpenTerminals,
}

impl Terminals {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            open: Arc::default(),
        }
    }

    pub fn open(
        &self,
        key: &str,
        cols: u16,
        rows: u16,
        emit: impl Fn(Value) + Send + 'static,
    ) -> Result<Value, String> {
        let shell = user_shell();
        let mut command = CommandBuilder::new(&shell);
        #[cfg(unix)]
        command.arg("-l");
        command.cwd(&self.workspace);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        self.spawn(key, cols, rows, command, emit)?;
        Ok(
            json!({ "shell": shell.file_name().unwrap_or_default().to_string_lossy(), "workspace": self.workspace }),
        )
    }

    fn spawn(
        &self,
        key: &str,
        cols: u16,
        rows: u16,
        command: CommandBuilder,
        emit: impl Fn(Value) + Send + 'static,
    ) -> Result<(), String> {
        validate_key(key)?;
        let size = size(cols, rows)?;
        let mut open = self.open.lock().map_err(|error| error.to_string())?;
        if open.contains_key(key) {
            return Err("This terminal is already open".into());
        }
        if open.len() >= 4 {
            return Err("Close a terminal before opening another (maximum four)".into());
        }
        let (terminal, mut reader) = spawn(command, size)?;
        let generation = Arc::clone(&terminal.generation);
        open.insert(key.to_owned(), terminal);
        let terminals = Arc::downgrade(&self.open);
        let key = key.to_owned();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => emit(json!({ "kind": "output", "data": &buffer[..length] })),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let terminal = terminals.upgrade().and_then(|open| {
                let mut open = open.lock().ok()?;
                if open
                    .get(&key)
                    .is_some_and(|terminal| Arc::ptr_eq(&terminal.generation, &generation))
                {
                    open.remove(&key)
                } else {
                    None
                }
            });
            let code = terminal
                .and_then(|mut terminal| terminal.child.take())
                .and_then(|mut child| child.wait().ok())
                .map(|status| status.exit_code());
            emit(json!({ "kind": "exit", "code": code }));
        });
        Ok(())
    }

    pub fn write(&self, key: &str, data: &str) -> Result<(), String> {
        if data.len() > 65536 {
            return Err("Terminal input is too large".into());
        }
        // A blocked paste must not prevent another tab resizing or closing.
        let input = self
            .open
            .lock()
            .map_err(|error| error.to_string())?
            .get(key)
            .map(|terminal| Arc::clone(&terminal.input))
            .ok_or("This terminal has exited")?;
        let mut input = input.lock().map_err(|error| error.to_string())?;
        input
            .write_all(data.as_bytes())
            .and_then(|()| input.flush())
            .map_err(|error| format!("Could not write to terminal: {error}"))
    }

    pub fn resize(&self, key: &str, cols: u16, rows: u16) -> Result<(), String> {
        let size = size(cols, rows)?;
        let open = self.open.lock().map_err(|error| error.to_string())?;
        open.get(key)
            .ok_or("This terminal has exited")?
            .master
            .resize(size)
            .map_err(|error| format!("Could not resize terminal: {error}"))
    }

    pub fn close(&self, key: &str) -> Result<(), String> {
        let terminal = self
            .open
            .lock()
            .map_err(|error| error.to_string())?
            .remove(key);
        drop(terminal);
        Ok(())
    }

    pub fn close_all(&self) {
        if let Ok(mut open) = self.open.lock() {
            open.clear();
        }
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn spawn(command: CommandBuilder, size: PtySize) -> Result<(Terminal, Output), String> {
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|error| error.to_string())?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| error.to_string())?;
    let input = pair
        .master
        .take_writer()
        .map_err(|error| error.to_string())?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Could not start shell: {error}"))?;
    drop(pair.slave);
    Ok((
        Terminal {
            master: pair.master,
            input: Arc::new(Mutex::new(input)),
            child: Some(child),
            generation: Arc::new(()),
        },
        reader,
    ))
}

fn size(cols: u16, rows: u16) -> Result<PtySize, String> {
    if !(2..=500).contains(&cols) || !(2..=250).contains(&rows) {
        return Err("Invalid terminal dimensions".into());
    }
    Ok(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    })
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid terminal key".into());
    }
    Ok(())
}

fn user_shell() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|shell| shell.is_absolute() && shell.is_file())
            .unwrap_or_else(|| {
                PathBuf::from(if cfg!(target_os = "macos") {
                    "/bin/zsh"
                } else {
                    "/bin/sh"
                })
            })
    }
    #[cfg(windows)]
    {
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| "cmd.exe".into())
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
