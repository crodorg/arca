//! Thin RPC client over Unix socket or TCP. One request per connection.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::{TcpStream, UnixStream};

use arca_core::rpc::{Request, Response, read_response, write_request};

pub enum Transport<'a> {
    Unix(&'a Path),
    Tcp(&'a str),
}

/// Cap on a single connect+request+response round-trip. A live daemon answers
/// in milliseconds; this only bites when the socket is accepted but the daemon
/// hangs, so the TUI surfaces an error instead of freezing the event loop.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn call(transport: Transport<'_>, req: &Request) -> Result<Response> {
    tokio::time::timeout(RPC_TIMEOUT, call_inner(transport, req))
        .await
        .with_context(|| {
            format!(
                "timed out after {}s — is arca-daemon running?",
                RPC_TIMEOUT.as_secs()
            )
        })?
}

async fn call_inner(transport: Transport<'_>, req: &Request) -> Result<Response> {
    match transport {
        Transport::Unix(p) => {
            let stream = UnixStream::connect(p).await?;
            let (mut r, mut w) = stream.into_split();
            write_request(&mut w, req).await?;
            Ok(read_response(&mut r).await?)
        }
        Transport::Tcp(addr) => {
            let stream = TcpStream::connect(addr).await?;
            let (mut r, mut w) = stream.into_split();
            write_request(&mut w, req).await?;
            Ok(read_response(&mut r).await?)
        }
    }
}
