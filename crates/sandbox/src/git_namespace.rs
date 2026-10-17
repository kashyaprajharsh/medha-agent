use crate::{exec::ExecError, git_controls::Protected};
use std::{ffi::CString, io, os::unix::ffi::OsStrExt, sync::Arc};

pub(crate) struct Mounts {
    entries: Vec<CString>,
    readonly: Vec<CString>,
    uid: CString,
    gid: CString,
}

impl Mounts {
    pub fn new(protected: &[Arc<Protected>]) -> Result<Self, ExecError> {
        let convert = |path: &std::path::Path| {
            CString::new(path.as_os_str().as_bytes())
                .map_err(|_| ExecError::Unavailable("Git mount path contains NUL".into()))
        };
        Ok(Self {
            entries: protected
                .iter()
                .map(|item| convert(&item._entry))
                .collect::<Result<_, _>>()?,
            readonly: protected
                .iter()
                .flat_map(|item| &item.readonly)
                .map(|path| convert(path))
                .collect::<Result<_, _>>()?,
            uid: CString::new(format!("{} {} 1\n", unsafe { libc::geteuid() }, unsafe {
                libc::geteuid()
            }))
            .unwrap(),
            gid: CString::new(format!("{} {} 1\n", unsafe { libc::getegid() }, unsafe {
                libc::getegid()
            }))
            .unwrap(),
        })
    }

    pub fn install(&self) -> io::Result<()> {
        // All paths and mapping bytes were prepared before fork.
        unsafe {
            checked(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            write_map(c"/proc/self/setgroups", b"deny")?;
            write_map(c"/proc/self/uid_map", self.uid.as_bytes())?;
            write_map(c"/proc/self/gid_map", self.gid.as_bytes())?;
            checked(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            for entry in &self.entries {
                checked(libc::mount(
                    entry.as_ptr(),
                    entry.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ))?;
            }
            for path in &self.readonly {
                checked(libc::mount(
                    path.as_ptr(),
                    path.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ))?;
                #[repr(C)]
                struct Attributes {
                    set: u64,
                    clear: u64,
                    propagation: u64,
                    userns_fd: u64,
                }
                let attributes = Attributes {
                    set: 1,
                    clear: 0,
                    propagation: 0,
                    userns_fd: 0,
                };
                checked(libc::syscall(
                    libc::SYS_mount_setattr,
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    0x8000u32,
                    &attributes,
                    std::mem::size_of::<Attributes>(),
                ) as i32)?;
            }
            #[repr(C)]
            struct Header {
                version: u32,
                pid: i32,
            }
            #[repr(C)]
            #[derive(Clone, Copy)]
            struct Data {
                effective: u32,
                permitted: u32,
                inheritable: u32,
            }
            let header = Header {
                version: 0x20080522,
                pid: 0,
            };
            let data = [Data {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            }; 2];
            checked(libc::syscall(libc::SYS_capset, &header, data.as_ptr()) as i32)?;
        }
        Ok(())
    }
}

/// Whether this system lets an ordinary program set up the private view the
/// mounts need. Ubuntu 24.04 and some hardened systems refuse it by default.
pub(crate) fn available() -> bool {
    use std::os::unix::process::CommandExt;
    #[cfg(test)]
    if REFUSED.with(std::cell::Cell::get) {
        return false;
    }
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let Ok(probe) = Mounts::new(&[]) else {
            return false;
        };
        let mut command = std::process::Command::new("true");
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(move || probe.install());
        }
        command.status().is_ok_and(|status| status.success())
    })
}

// A test sets this, on its own thread only, to take a refusing system's path.
#[cfg(test)]
thread_local! {
    pub(crate) static REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

unsafe fn write_map(path: &std::ffi::CStr, bytes: &[u8]) -> io::Result<()> {
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        let error = (written != bytes.len() as isize).then(io::Error::last_os_error);
        libc::close(fd);
        error.map_or(Ok(()), Err)
    }
}
