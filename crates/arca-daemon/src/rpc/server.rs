//! Tokio Unix-socket + TCP listeners. One request per connection (v1).
//!
//! Two Unix sockets split the surface: a **read** socket (`snapshot.*`,
//! `tx.list`, `health`) that the arca-xmpp bridge uses, and a **write** socket
//! that adds `manual.*` and `provider.refresh`. Write verbs arriving on the read
//! socket are rejected at dispatch; the write socket additionally gates every
//! connection to the operator's UID via `getpeereid(2)` (see `peercred`). The
//! TCP listener serves all verbs with **no** peer-UID auth (UID is meaningless
//! over TCP) and is therefore loopback-only — `bind_tcp` rejects any
//! non-loopback bind at startup. It is latent/undeployed in v1; remote access is
//! the mesh-SSH TUI to the UID-gated write socket, not direct TCP.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener, UnixStream};

use arca_core::rpc::{Request, Response, RpcError, read_request, write_response};

use crate::peercred;

use super::handler::{State, handle, handle_refresh};

/// Which verbs a listener accepts, and whether it enforces the operator UID.
#[derive(Clone, Copy, Debug)]
pub enum SocketRole {
    /// Read-only verbs. Write verbs are rejected at dispatch. No UID gate.
    Read,
    /// All verbs. On a Unix socket, the peer UID must match the operator.
    Write,
}

/// Bind a Unix socket with an explicit mode, up front, so a failure aborts the
/// daemon at startup (the design spec: honest failure, no silent fallbacks).
pub fn bind_unix(path: &std::path::Path, mode: u32) -> anyhow::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(listener)
}

pub async fn serve_unix(
    state: Arc<State>,
    listener: UnixListener,
    path: PathBuf,
    role: SocketRole,
    operator_uid: Option<u32>,
) -> anyhow::Result<()> {
    tracing::info!(socket = %path.display(), role = ?role, "unix RPC listening");
    loop {
        let (stream, _addr) = listener.accept().await?;
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            if let SocketRole::Write = role {
                if let Err(reason) = check_operator(&stream, operator_uid) {
                    reject(stream, reason).await;
                    return;
                }
            }
            let (r, w) = stream.into_split();
            if let Err(e) = handle_conn(st, r, w, role).await {
                tracing::warn!(error = %e, "unix conn");
            }
        });
    }
}

/// Enforce the operator-UID gate on the write socket. Returns the rejection
/// reason on failure (caller closes the connection with an error frame).
fn check_operator(stream: &UnixStream, operator_uid: Option<u32>) -> Result<(), String> {
    let uid =
        peercred::peer_uid(stream).map_err(|e| format!("peer credential check failed: {e}"))?;
    operator_gate(uid, operator_uid)
}

/// Pure auth decision for the write socket: accept iff `operator_uid` is set and
/// equals the peer `uid`. An unset `operator_uid` **fails closed** (rejects
/// all). Split out from [`check_operator`] so the comparison + fail-closed
/// branch are unit-testable without a real socket.
fn operator_gate(uid: u32, operator_uid: Option<u32>) -> Result<(), String> {
    match operator_uid {
        Some(op) if uid == op => Ok(()),
        Some(op) => Err(format!(
            "write socket is operator-only (peer uid {uid} != operator uid {op})"
        )),
        None => Err("write socket: operator_uid not configured (failing closed)".into()),
    }
}

/// Close a rejected write-socket connection with a `forbidden` error frame so
/// the client sees a reason instead of a bare disconnect.
async fn reject(stream: UnixStream, reason: String) {
    tracing::warn!(reason = %reason, "write socket: connection rejected");
    let (_r, mut w) = stream.into_split();
    let resp = Response::Error(RpcError {
        code: "forbidden".into(),
        msg: reason,
    });
    let _ = write_response(&mut w, &resp).await;
}

/// The TCP listener serves the full write verb set with **no** peer-UID auth
/// (peer UID is meaningless over TCP). That is safe *only* on loopback. A
/// non-loopback bind — a config typo like `0.0.0.0` or the tailnet IP — would
/// silently expose unauthenticated, writable RPC. Reject it at startup (honest
/// failure). the design spec: never bind the daemon to the tailnet IP; remote access
/// is mesh SSH, not direct TCP, and 7732 is never exposed.
fn require_loopback(bind: &str) -> anyhow::Result<()> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = bind
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("resolve tcp_bind {bind:?}: {e}"))?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("tcp_bind {bind:?} resolved to no addresses");
    }
    if let Some(bad) = addrs.iter().find(|a| !a.ip().is_loopback()) {
        anyhow::bail!(
            "tcp_bind {bind:?} resolves to non-loopback {bad}: the TCP listener has \
             no peer-UID auth and must bind loopback only (remote access is mesh \
             SSH; never expose 7732)"
        );
    }
    Ok(())
}

pub async fn bind_tcp(bind: &str) -> anyhow::Result<TcpListener> {
    require_loopback(bind)?;
    let listener = TcpListener::bind(bind).await?;
    Ok(listener)
}

pub async fn serve_tcp(
    state: Arc<State>,
    listener: TcpListener,
    bind: String,
) -> anyhow::Result<()> {
    tracing::info!(addr = %bind, "tcp RPC listening");
    loop {
        let (stream, addr) = listener.accept().await?;
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            let (r, w) = stream.into_split();
            // Full verb set, no peer-UID gate (UID is meaningless over TCP). Safe
            // only because `bind_tcp` rejects any non-loopback bind at startup —
            // nothing remote reaches this (no pf `pass in` for 7732; remote access
            // is the mesh-SSH TUI on the UID-gated write socket).
            if let Err(e) = handle_conn(st, r, w, SocketRole::Write).await {
                tracing::warn!(error = %e, peer = %addr, "tcp conn");
            }
        });
    }
}

async fn handle_conn<R, W>(
    state: Arc<State>,
    mut r: R,
    mut w: W,
    role: SocketRole,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let req = read_request(&mut r).await?;
    if matches!(role, SocketRole::Read) && req.is_write() {
        let resp = Response::Error(RpcError {
            code: "forbidden".into(),
            msg: "write verb rejected on read socket".into(),
        });
        write_response(&mut w, &resp).await?;
        return Ok(());
    }
    let resp = match req {
        Request::ProviderRefresh { kind_filter } => handle_refresh(&state, kind_filter).await,
        other => {
            tokio::task::spawn_blocking({
                let st = Arc::clone(&state);
                move || handle(&st, other)
            })
            .await?
        }
    };
    write_response(&mut w, &resp).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::db::Db;
    use arca_core::rpc::{read_response, write_request};
    use std::time::Instant;
    use tokio::io::{duplex, split};

    #[test]
    fn require_loopback_rejects_non_loopback() {
        // A config typo that would silently expose unauthenticated writable RPC.
        assert!(require_loopback("0.0.0.0:7732").is_err());
        assert!(require_loopback("192.168.1.1:7732").is_err());
        assert!(require_loopback("100.64.0.5:7732").is_err()); // tailnet IP
        assert!(require_loopback("[::]:7732").is_err());
    }

    #[test]
    fn require_loopback_accepts_loopback() {
        assert!(require_loopback("127.0.0.1:7732").is_ok());
        assert!(require_loopback("127.0.0.1:0").is_ok());
        assert!(require_loopback("[::1]:7732").is_ok());
    }

    #[test]
    fn operator_gate_accepts_only_matching_uid_and_fails_closed() {
        assert!(operator_gate(1000, Some(1000)).is_ok());
        assert!(operator_gate(1001, Some(1000)).is_err()); // wrong uid
        assert!(operator_gate(0, Some(1000)).is_err()); // root is not the operator
        assert!(operator_gate(1000, None).is_err()); // unset → fail closed
        assert!(operator_gate(0, None).is_err());
    }

    fn test_state() -> Arc<State> {
        Arc::new(State {
            db: Arc::new(Db::open_memory().expect("memory db")),
            started_at: Instant::now(),
            version: "test",
            providers: Arc::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn read_socket_rejects_write_verb() {
        let (client, server) = duplex(8192);
        let (sr, sw) = split(server);
        let task = tokio::spawn(handle_conn(test_state(), sr, sw, SocketRole::Read));
        let (mut cr, mut cw) = split(client);
        write_request(
            &mut cw,
            &Request::ManualUpsertBusiness {
                tag: "acme".into(),
                display_name: None,
                active: None,
            },
        )
        .await
        .unwrap();
        match read_response(&mut cr).await.unwrap() {
            Response::Error(e) => assert_eq!(e.code, "forbidden"),
            other => panic!("expected forbidden, got {other:?}"),
        }
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn read_socket_allows_read_verb() {
        let (client, server) = duplex(8192);
        let (sr, sw) = split(server);
        let task = tokio::spawn(handle_conn(test_state(), sr, sw, SocketRole::Read));
        let (mut cr, mut cw) = split(client);
        write_request(&mut cw, &Request::Health).await.unwrap();
        assert!(matches!(
            read_response(&mut cr).await.unwrap(),
            Response::Health(_)
        ));
        task.await.unwrap().unwrap();
    }
}
