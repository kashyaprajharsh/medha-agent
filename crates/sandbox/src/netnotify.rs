//! Exact network-denial detection on Linux, from the kernel rather than from
//! whatever text a blocked program happened to print.
//!
//! A `SECCOMP_RET_USER_NOTIF` filter in the child traps `connect` and hands a
//! notification file descriptor back to this process. The first attempt to reach
//! an IP address is therefore observed the moment it is made: before any output
//! exists, whatever language the program is written in, and — the reason this
//! exists at all — even when its stderr was piped into a filter whose buffer the
//! kill never flushes.
//!
//! This is a *detector*, not the security boundary. Landlock still denies the
//! network; every response here mirrors what Landlock would have done anyway.
//! That distinction is what makes `SECCOMP_USER_NOTIF_FLAG_CONTINUE` acceptable
//! for the addresses we let through: a target that raced us into swapping a
//! `sockaddr` after inspection would still meet Landlock on the way out.
//!
//! Only `connect` is trapped. Its address argument sits at a fixed index, which
//! `sendmsg`'s (buried in a `msghdr`) does not, and it is the operation every
//! TCP client must perform. DNS over UDP keeps working exactly as before.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;

/// `AUDIT_ARCH_*` for the architecture we were built for, paired with that
/// architecture's `connect` number. A filter that did not pin the architecture
/// could be evaded by a process re-entering through a different syscall ABI.
#[cfg(target_arch = "x86_64")]
const ARCH_AND_CONNECT: (u32, u32) = (0xc000_003e, 42);
#[cfg(target_arch = "aarch64")]
const ARCH_AND_CONNECT: (u32, u32) = (0xc000_00b7, 203);

#[repr(C)]
#[derive(Default)]
struct SeccompData {
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Default)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

const _: () = assert!(std::mem::size_of::<SeccompNotif>() == 80);
const _: () = assert!(std::mem::size_of::<SeccompNotifResp>() == 24);

/// `_IOWR('!', nr, size)` — the encoding the seccomp notification ioctls use.
const fn iowr(nr: u32, size: u32) -> libc::c_ulong {
    ((3 << 30) | (size << 16) | ((b'!' as u32) << 8) | nr) as libc::c_ulong
}

const NOTIF_RECV: libc::c_ulong = iowr(0, 80);
const NOTIF_SEND: libc::c_ulong = iowr(1, 24);

/// How long the watcher waits for the child to hand over its listener. A child
/// that failed to install a filter never sends one, and detection simply falls
/// back to reading the command's output.
const HANDOFF_TIMEOUT: libc::time_t = 2;
/// Poll slice, so the watcher notices a finished command promptly without
/// blocking forever in `NOTIF_RECV` once the last target is gone.
const POLL_SLICE_MS: libc::c_int = 250;

/// Parent-side end of an armed filter, returned by [`arm`] and consumed by
/// [`watch`] once the child exists.
pub(crate) struct Pending {
    parent: OwnedFd,
    flag: Arc<AtomicBool>,
}

impl Pending {
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
}

/// Register a pre-exec step that installs the filter in the child, and return
/// the parent's half of the handoff. `None` when this build has no filter for
/// the running architecture, in which case nothing is installed at all.
pub(crate) fn arm(cmd: &mut std::process::Command) -> Option<Pending> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = cmd;
        return None;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        use std::os::unix::process::CommandExt;

        let (parent, child) = socketpair()?;
        // Built here, in the parent, where allocation is safe: after the fork
        // only the address of this buffer is used.
        let program = build_filter();
        let child_fd = child.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                install_in_child(&program, child_fd);
                // Never fatal. A kernel that refuses the filter leaves the
                // command running exactly as it would have without this.
                Ok(())
            });
        }
        // Keep the child's end alive until the fork; the closure only borrows
        // its number, so it must not be closed before `spawn`.
        std::mem::forget(child);
        Some(Pending {
            parent,
            flag: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// Start the watcher thread. It owns the handoff socket and exits when the
/// command finishes or the last target is gone.
pub(crate) fn watch(pending: Pending, finished: Arc<AtomicBool>) {
    let Pending { parent, flag } = pending;
    let spawned = std::thread::Builder::new()
        .name("medha-netnotify".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let Some(listener) = receive_listener(parent.as_raw_fd()) else {
                return;
            };
            supervise(listener, &flag, &finished);
        });
    // A thread we could not start simply means no exact detection; the output
    // scanner still runs.
    let _ = spawned;
}

fn build_filter() -> Vec<libc::sock_filter> {
    let (audit_arch, nr_connect) = ARCH_AND_CONNECT;
    // offsetof(struct seccomp_data, arch) == 4, offsetof(.., nr) == 0.
    let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;
    vec![
        stmt(load, 4),
        jeq(audit_arch, 0, 3),
        stmt(load, 0),
        jeq(nr_connect, 0, 1),
        stmt(ret, SECCOMP_RET_USER_NOTIF),
        stmt(ret, SECCOMP_RET_ALLOW),
    ]
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jeq(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    }
}

fn socketpair() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // Not CLOEXEC: the child half must survive to the pre-exec step. It is
    // closed there, before exec, so it never reaches the command itself.
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let timeout = libc::timeval {
        tv_sec: HANDOFF_TIMEOUT,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fds[0],
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        Some((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

/// Runs after `fork`, before `exec`: async-signal-safe only. No allocation, no
/// locks — just the syscalls, and silence on any failure.
fn install_in_child(program: &[libc::sock_filter], handoff: RawFd) {
    unsafe {
        // Landlock already sets this, but the filter install requires it and
        // this must not depend on the order pre-exec steps were registered in.
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        let prog = libc::sock_fprog {
            len: program.len() as u16,
            filter: program.as_ptr() as *mut libc::sock_filter,
        };
        let listener = libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            (&raw const prog).cast::<libc::c_void>(),
        );
        if listener >= 0 {
            send_fd(handoff, listener as RawFd);
            // Critical: drop our own reference. While a target holds the
            // listener the kernel believes a supervisor exists, so a trapped
            // syscall would wait forever for an answer from the very process
            // that is blocked on it.
            libc::close(listener as RawFd);
        }
        libc::close(handoff);
    }
}

unsafe fn send_fd(sock: RawFd, fd: RawFd) {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 32];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return;
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        libc::sendmsg(sock, &msg, 0);
    }
}

fn receive_listener(sock: RawFd) -> Option<OwnedFd> {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 32];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        if libc::recvmsg(sock, &mut msg, 0) < 0 {
            return None;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return None;
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
        (fd >= 0).then(|| OwnedFd::from_raw_fd(fd))
    }
}

fn supervise(listener: OwnedFd, flag: &AtomicBool, finished: &AtomicBool) {
    let fd = listener.as_raw_fd();
    while !finished.load(Ordering::Relaxed) {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, POLL_SLICE_MS) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if ready == 0 {
            continue;
        }
        if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return;
        }
        let mut notif = SeccompNotif::default();
        if unsafe { libc::ioctl(fd, NOTIF_RECV, &raw mut notif) } != 0 {
            // ENOENT: the target died between the trap and this read. Anything
            // else means the listener is unusable.
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT) | Some(libc::EINTR) => continue,
                _ => return,
            }
        }
        let remote = is_remote_address(notif.pid, notif.data.args[1]);
        if remote {
            flag.store(true, Ordering::Relaxed);
        }
        respond(fd, &notif, remote);
    }
}

/// Read the `sa_family` of the `sockaddr` the target passed. Anything we cannot
/// read is treated as local: a failed read must not manufacture a grant prompt
/// for a command that never touched the network.
fn is_remote_address(pid: u32, addr: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut mem) = std::fs::File::open(format!("/proc/{pid}/mem")) else {
        return false;
    };
    if mem.seek(SeekFrom::Start(addr)).is_err() {
        return false;
    }
    let mut family = [0u8; 2];
    if mem.read_exact(&mut family).is_err() {
        return false;
    }
    let family = u16::from_ne_bytes(family) as i32;
    family == libc::AF_INET || family == libc::AF_INET6
}

fn respond(fd: RawFd, notif: &SeccompNotif, remote: bool) {
    let resp = if remote {
        // Mirror Landlock's own verdict for a denied connect, so the target sees
        // one consistent failure whether or not this layer is present.
        SeccompNotifResp {
            id: notif.id,
            val: 0,
            error: -libc::EACCES,
            flags: 0,
        }
    } else {
        SeccompNotifResp {
            id: notif.id,
            val: 0,
            error: 0,
            flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
        }
    };
    unsafe {
        libc::ioctl(fd, NOTIF_SEND, &raw const resp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_encoding() {
        // Verified against a live kernel by the probe that preceded this module.
        assert_eq!(NOTIF_RECV, 0xc050_2100);
        assert_eq!(NOTIF_SEND, 0xc018_2101);
    }

    #[test]
    fn the_filter_pins_the_architecture_before_reading_the_syscall() {
        let program = build_filter();
        assert_eq!(program.len(), 6);
        assert_eq!(program[0].k, 4, "first load must be seccomp_data.arch");
        assert_eq!(program[1].k, ARCH_AND_CONNECT.0);
        assert_eq!(
            program[1].jf, 3,
            "a foreign architecture must skip to the trap-free tail"
        );
        assert_eq!(program[2].k, 0, "second load must be seccomp_data.nr");
        assert_eq!(program[3].k, ARCH_AND_CONNECT.1);
        assert_eq!(program[4].k, SECCOMP_RET_USER_NOTIF);
        assert_eq!(program[5].k, SECCOMP_RET_ALLOW);
    }

    #[test]
    fn an_unreadable_target_is_never_reported_as_remote() {
        assert!(
            !is_remote_address(u32::MAX, 0),
            "a failed read must not manufacture a network denial"
        );
    }

    /// The whole point: the command's stderr goes to /dev/null, so there is no
    /// text for any scanner to match, and detection still happens — from the
    /// kernel, while the command is still running.
    #[tokio::test]
    async fn a_denied_connect_is_seen_with_no_output_whatsoever() {
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            // Silence the shell itself first, so its own diagnostic about the
            // refused connect never reaches our capture either.
            .arg("exec 2>/dev/null; exec 3<>/dev/tcp/93.184.216.34/80; sleep 5");
        let process = crate::exec::spawn_background(command, true).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !process.network_denial_seen() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            process.network_denial_seen(),
            "a connect under a net-denying jail must be observed"
        );
        let (stdout, stderr) = process.snapshot();
        assert!(
            stdout.is_empty() && stderr.is_empty(),
            "premise of this test is that no output exists: {stdout:?} / {stderr:?}"
        );
        assert!(
            process.is_running(),
            "detection must not have required the command to exit"
        );
        process.kill();
        process.wait().await;
    }

    /// A command that only touches unix sockets must be waved through. Getting
    /// this wrong breaks every command on the platform, not just networked ones.
    #[tokio::test]
    async fn local_only_work_is_untouched_by_the_filter() {
        let mut command = tokio::process::Command::new("bash");
        // getent walks nsswitch, which talks to local unix sockets, and `id`
        // resolves users the same way. Both must complete normally.
        command
            .arg("-c")
            .arg("getent passwd root >/dev/null && id >/dev/null && echo fine");
        let process = crate::exec::spawn_background(command, true).unwrap();
        process.wait().await;
        let (stdout, _stderr) = process.snapshot();
        assert!(
            stdout.contains("fine"),
            "local work was broken by the filter: {stdout:?}"
        );
        assert!(
            !process.network_denial_seen(),
            "unix-socket work must not be reported as a network denial"
        );
        assert_eq!(process.exit_code(), Some(0));
    }
}
