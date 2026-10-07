//! The user's terminal, independent of Medha's agent executor.
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

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

struct Slot {
    local: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.local.fetch_sub(1, Ordering::AcqRel);
        self.total.fetch_sub(1, Ordering::AcqRel);
    }
}

fn total_slots() -> &'static Arc<AtomicUsize> {
    static TOTAL: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    TOTAL.get_or_init(Arc::default)
}

pub(crate) fn all_reaped() -> bool {
    total_slots().load(Ordering::Acquire) == 0
}

fn reserve_slot(local: &Arc<AtomicUsize>) -> Result<Arc<Slot>, String> {
    let total = total_slots();
    local.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < 4).then_some(count + 1))
        .map_err(|_| "At most four terminals per workspace can be open or closing. Wait for a closing shell to exit.")?;
    if total
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 16).then_some(count + 1)
        })
        .is_err()
    {
        local.fetch_sub(1, Ordering::AcqRel);
        return Err(
            "At most sixteen terminals can be open or closing. Wait for a closing shell to exit."
                .into(),
        );
    }
    Ok(Arc::new(Slot {
        local: Arc::clone(local),
        total: Arc::clone(total),
    }))
}

struct Terminal {
    master: Option<Box<dyn MasterPty + Send>>,
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    generation: Arc<()>,
    reaped: Arc<Reaped>,
    slot: Option<Arc<Slot>>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let master = self.master.take();
            let input = Arc::clone(&self.input);
            let reaped = Arc::clone(&self.reaped);
            let slot = self.slot.take();
            // Signal, reap and terminal-handle destruction can all block in
            // the OS. None of them belong on the app thread or map lock.
            std::thread::spawn(move || {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    #[cfg(unix)]
                    {
                        if let Some(group) = master
                            .as_ref()
                            .and_then(|master| master.process_group_leader())
                            .filter(|group| *group > 1 && Some(*group as u32) != child.process_id())
                        {
                            // A foreground job may have its own process group.
                            unsafe {
                                libc::kill(-group, libc::SIGHUP);
                            }
                        }
                    }
                    // portable-pty gives Unix shells SIGHUP and a short grace
                    // period before SIGKILL; Windows uses TerminateProcess.
                    let _ = child.kill();
                    let _ = child.wait();
                }
                reaped.finish();
                drop(input);
                drop(master);
                drop(slot);
            });
        }
    }
}

pub struct Terminals {
    workspace: PathBuf,
    open: OpenTerminals,
    occupied: Arc<AtomicUsize>,
}

impl Terminals {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            open: Arc::default(),
            occupied: Arc::default(),
        }
    }

    pub fn open(
        &self,
        key: &str,
        cols: u16,
        rows: u16,
        generation: Arc<()>,
        emit: impl Fn(Value) + Send + 'static,
    ) -> Result<Value, String> {
        let shell = user_shell();
        let mut command = CommandBuilder::new(&shell);
        #[cfg(unix)]
        command.arg("-l");
        command.cwd(&self.workspace);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        self.spawn_in(
            key,
            (cols, rows),
            generation,
            move |size| spawn(command, size),
            emit,
        )?;
        Ok(
            json!({ "shell": shell.file_name().unwrap_or_default().to_string_lossy(), "workspace": self.workspace }),
        )
    }

    #[cfg(test)]
    fn spawn(
        &self,
        key: &str,
        cols: u16,
        rows: u16,
        command: CommandBuilder,
        emit: impl Fn(Value) + Send + 'static,
    ) -> Result<(), String> {
        self.spawn_in(
            key,
            (cols, rows),
            Arc::new(()),
            move |size| spawn(command, size),
            emit,
        )
    }

    fn spawn_in(
        &self,
        key: &str,
        dimensions: (u16, u16),
        generation: Arc<()>,
        start: impl FnOnce(PtySize) -> Result<(Terminal, Output), String>,
        emit: impl Fn(Value) + Send + 'static,
    ) -> Result<(), String> {
        validate_key(key)?;
        let size = size(dimensions.0, dimensions.1)?;
        let mut open = self.open.lock().map_err(|error| error.to_string())?;
        if open.contains_key(key) {
            return Err("This terminal is already open".into());
        }
        let slot = reserve_slot(&self.occupied)?;
        // A reservation can be removed while native spawn is still blocked.
        // Closing never waits for spawn and the eventual child is retired.
        open.insert(
            key.to_owned(),
            Terminal {
                master: None,
                input: Arc::new(Mutex::new(Box::new(std::io::sink()))),
                child: None,
                generation: Arc::clone(&generation),
                reaped: Arc::default(),
                slot: Some(Arc::clone(&slot)),
            },
        );
        drop(open);
        let (mut terminal, mut reader) = match start(size) {
            Ok(pair) => pair,
            Err(error) => {
                self.close_generation(key, &generation)?;
                return Err(error);
            }
        };
        terminal.slot = Some(Arc::clone(&slot));
        terminal.generation = Arc::clone(&generation);
        let reaped = Arc::clone(&terminal.reaped);
        let active = {
            let mut open = self.open.lock().map_err(|error| error.to_string())?;
            if open
                .get(key)
                .is_some_and(|old| Arc::ptr_eq(&old.generation, &generation))
            {
                open.insert(key.to_owned(), terminal);
                true
            } else {
                drop(open);
                drop(terminal);
                false
            }
        };
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
                // Natural EOF can precede reaping. Keep all terminal handles
                // alive until wait has completed, just as on explicit close.
                let status = terminal.child.as_mut()?.wait().ok();
                terminal.child.take();
                reaped.finish();
                status.map(|status| status.exit_code())
            });
            reaped.wait();
            drop(reader);
            drop(slot);
            emit(json!({ "kind": "exit", "code": code }));
        });
        if active {
            Ok(())
        } else {
            Err("This terminal was closed while its shell was starting".into())
        }
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
            .filter(|terminal| terminal.child.is_some())
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

    #[cfg(test)]
    pub fn close(&self, key: &str) -> Result<(), String> {
        let terminal = self
            .open
            .lock()
            .map_err(|error| error.to_string())?
            .remove(key);
        drop(terminal);
        Ok(())
    }

    pub fn close_generation(&self, key: &str, generation: &Arc<()>) -> Result<(), String> {
        let terminal = {
            let mut open = self.open.lock().map_err(|error| error.to_string())?;
            if open
                .get(key)
                .is_some_and(|terminal| Arc::ptr_eq(&terminal.generation, generation))
            {
                open.remove(key)
            } else {
                None
            }
        };
        drop(terminal);
        Ok(())
    }

    pub fn close_all(&self) {
        let terminals = self
            .open
            .lock()
            .map(|mut open| std::mem::take(&mut *open))
            .unwrap_or_default();
        drop(terminals);
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
            slot: None,
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
