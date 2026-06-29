//! Peer-credential lookup for Unix sockets.
//!
//! The write socket is gated to the operator's UID: the `_arca` daemon cannot
//! create a `user`-owned socket (only root chowns across users), so mode bits
//! alone can't distinguish the operator from anything else running as `_arca`
//! (notably the arca-xmpp bridge). We ask the kernel for the connecting peer's
//! effective UID, which is real enforcement regardless of socket mode — a
//! compromised bridge running as `_arca` still cannot write.
//!
//! Two kernels, two syscalls (both expose the same fact). OpenBSD — the
//! production target — and the other BSDs/macOS use `getpeereid(2)`. Linux has
//! no `getpeereid` symbol in libc, so we read `SO_PEERCRED` (`struct ucred`).
//! We declare the FFI directly rather than pull in `libc`, matching `pledge.rs`.

use std::io;
use std::os::raw::c_int;
use std::os::unix::io::AsRawFd;

/// Effective UID of the peer connected on `sock`. Errors if the fd is not a
/// connected socket or the platform refuses the lookup (honest failure: the
/// caller rejects the connection rather than assuming a UID).
pub fn peer_uid<F: AsRawFd>(sock: &F) -> io::Result<u32> {
    peer_uid_fd(sock.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn peer_uid_fd(fd: c_int) -> io::Result<u32> {
    use std::os::raw::c_void;

    #[repr(C)]
    struct Ucred {
        _pid: i32,
        uid: u32,
        _gid: u32,
    }
    const SOL_SOCKET: c_int = 1;
    const SO_PEERCRED: c_int = 17;

    unsafe extern "C" {
        fn getsockopt(
            sockfd: c_int,
            level: c_int,
            optname: c_int,
            optval: *mut c_void,
            optlen: *mut u32,
        ) -> c_int;
    }

    let mut cred = Ucred {
        _pid: 0,
        uid: 0,
        _gid: 0,
    };
    let mut len = u32::try_from(std::mem::size_of::<Ucred>()).expect("invariant: ucred fits u32");
    let rc = unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<c_void>(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc == 0 {
        Ok(cred.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn peer_uid_fd(fd: c_int) -> io::Result<u32> {
    unsafe extern "C" {
        fn getpeereid(s: c_int, euid: *mut u32, egid: *mut u32) -> c_int;
    }
    let mut uid: u32 = 0;
    let mut gid: u32 = 0;
    let rc = unsafe { getpeereid(fd, std::ptr::addr_of_mut!(uid), std::ptr::addr_of_mut!(gid)) };
    if rc == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    #[tokio::test]
    async fn peer_uid_matches_self() {
        // A socket to ourselves: both ends are this process, so the peer UID
        // the kernel reports must equal our own effective UID. Validates the
        // peer-credential FFI links and returns a sane value on this platform.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peercred.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let connect = UnixStream::connect(&path);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(connect, accept);
        let client = client.unwrap();
        let (server, _addr) = accepted.unwrap();

        let me = unsafe { geteuid() };
        assert_eq!(peer_uid(&server).unwrap(), me);
        assert_eq!(peer_uid(&client).unwrap(), me);
    }
}
