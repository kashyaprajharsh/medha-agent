//! The user's terminal, independent of Medha's agent executor.
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

type OpenTerminals = Arc<Mutex<HashMap<String, Terminal>>>;
type Output = Box<dyn Read + Send>;

#[derive(Default)]
struct Reaped {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Reaped {
    fn finish(&self) {
        *self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*done {
            done = self
                .changed
                .wait(done)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct Terminal {
    master: Option<Box<dyn MasterPty + Send>>,
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    generation: Arc<()>,
    reaped: Arc<Reaped>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Keep every master-side PTY handle alive until the shell is
            // reaped. Closing one while it is exiting stalls macOS teardown.
            let master = self.master.take();
            let input = Arc::clone(&self.input);
            let reaped = Arc::clone(&self.reaped);
            std::thread::spawn(move || {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    #[cfg(unix)]
                    {
                        // portable-pty's kill sends SIGHUP first. A shell
                        // still acquiring its controlling terminal can stall
                        // in that exit path on macOS. Stop the owned, unreaped
                        // shell directly, then wait before touching PTY state.
                        if let Some(pid) = child.process_id() {
                            unsafe {
                                libc::kill(pid as libc::pid_t, libc::SIGKILL);
                            }
                        } else {
                            let _ = child.kill();
                        }
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill();
                }
                let _ = child.wait();
                #[cfg(unix)]
                if let Some(group) = master
                    .as_ref()
                    .and_then(|master| master.process_group_leader())
                    .filter(|group| *group > 1 && Some(*group as u32) != child.process_id())
                {
                    // A remaining foreground job belongs to this terminal.
                    unsafe {
                        libc::kill(-group, libc::SIGHUP);
                    }
                }
                reaped.finish();
                drop(input);
                drop(master);
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
        let reaped = Arc::clone(&terminal.reaped);
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
            let code = terminal.and_then(|mut terminal| {
                // EOF can arrive before the shell has been reaped. Keep the
                // terminal (and its PTY handles) here until wait completes,
                // just as the explicit-close path does.
                let status = terminal.child.as_mut()?.wait().ok();
                terminal.child.take();
                reaped.finish();
                status.map(|status| status.exit_code())
            });
            // This clone of the master must also survive an explicit close
            // until the teardown worker has reaped the shell.
            reaped.wait();
            drop(reader);
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
            .as_ref()
            .ok_or("This terminal has exited")?
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
            master: Some(pair.master),
            input: Arc::new(Mutex::new(input)),
            child: Some(child),
            generation: Arc::new(()),
            reaped: Arc::default(),
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
