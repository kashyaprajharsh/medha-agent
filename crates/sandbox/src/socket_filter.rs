//! Linux seccomp filter for the sockets Landlock cannot mediate: Unix sockets always,
//! and every network family while the network is denied.

use crate::exec::NetPolicy;
use crate::netnotify::{ARCH_AND_CONNECT, jeq, stmt};

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_GET_ACTION_AVAIL: libc::c_ulong = 2;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// Built in the parent; the child only installs it.
pub(crate) fn program(net: NetPolicy) -> Vec<libc::sock_filter> {
    let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;
    let deny = stmt(ret, SECCOMP_RET_ERRNO | libc::EACCES as u32);
    let allow = stmt(ret, SECCOMP_RET_ALLOW);
    // Route lookups stay; a pair made by `socketpair` has no address and is not filtered.
    let mut families = vec![libc::AF_NETLINK];
    if net == NetPolicy::Allow {
        families.extend([libc::AF_INET, libc::AF_INET6]);
    }
    // A foreign syscall ABI numbers its calls differently, so it gets nothing.
    let mut filter = vec![
        stmt(load, 4),
        jeq(ARCH_AND_CONNECT.0, 1, 0),
        deny,
        stmt(load, 0),
    ];
    #[cfg(target_arch = "x86_64")]
    filter.extend([
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        deny,
    ]);
    filter.extend([
        // Ring submissions open sockets without passing through this filter.
        jeq(libc::SYS_io_uring_setup as u32, 0, 1),
        stmt(ret, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
        jeq(libc::SYS_socket as u32, 1, 0),
        allow,
        stmt(load, 16),
    ]);
    for (checked, family) in families.iter().enumerate() {
        filter.push(jeq(*family as u32, (families.len() - checked) as u8, 0));
    }
    filter.extend([deny, allow]);
    filter
}

/// Runs between `fork` and `exec`: one syscall, no allocation.
pub(crate) fn install(program: &[libc::sock_filter]) -> std::io::Result<()> {
    let prog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `prog` points at a live slice for the duration of the call.
    let installed = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0,
            (&raw const prog).cast::<libc::c_void>(),
        )
    };
    if installed == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn supported() -> bool {
    let action = SECCOMP_RET_ERRNO;
    // SAFETY: the kernel only reads the action value.
    unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_GET_ACTION_AVAIL,
            0,
            (&raw const action).cast::<libc::c_void>(),
        ) == 0
    }
}

#[cfg(test)]
#[path = "socket_filter_tests.rs"]
mod tests;
