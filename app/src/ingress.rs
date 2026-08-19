//! The loopback endpoint agents post to.
//!
//! Hand-written, and small enough to read in one sitting, because the only
//! client is our own hook: it always sends `content-length` and
//! `connection: close`, so none of the parts of HTTP that are hard to get right
//! ever arrive. `curl` behaves the same way, which is what keeps the endpoint
//! testable from a shell.
//!
//! Requests are served one at a time. At the observed peak — 3.4 hook spawns a
//! second — that is ample, and handling them in arrival order means events reach
//! the state machine in the order they happened without any sequencing
//! machinery.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};

/// Fixed rather than ephemeral: the hook command string we register must never
/// change, or Codex stops running it until the user re-approves.
pub const PORT: u16 = 47115;

/// Refuse anything larger than this rather than read it. The hook projects a
/// handful of short fields, so a large body means something is wrong, and a
/// serial server must never be parked reading it.
const MAX_BODY: usize = 64 * 1024;

pub struct Request {
    pub path: String,
    pub body: Vec<u8>,
}

pub fn bind() -> Result<TcpListener, String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);
    TcpListener::bind(addr).map_err(|error| match error.kind() {
        // Binding the port is also how we enforce a single instance: the thing
        // that must not be duplicated is the listener, so let it be the lock.
        std::io::ErrorKind::AddrInUse => {
            format!("port {PORT} is already in use — agent-frow is probably already running")
        }
        _ => format!("could not bind 127.0.0.1:{PORT}: {error}"),
    })
}

/// Serves until the listener fails. `on_request` returns the status to reply
/// with; the connection is closed either way.
pub fn serve(listener: &TcpListener, token: &str, mut on_request: impl FnMut(Request)) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        match read_request(stream, token) {
            Ok((mut stream, Some(request))) => {
                respond(&mut stream, 204, "No Content");
                on_request(request);
            }
            Ok((mut stream, None)) => respond(&mut stream, 401, "Unauthorized"),
            Err(_) => {}
        }
    }
}

type Accepted = (TcpStream, Option<Request>);

fn read_request(stream: TcpStream, token: &str) -> Result<Accepted, std::io::Error> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();

    let mut length = 0usize;
    let mut presented: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "x-agent-frow-token" => presented = Some(value.to_owned()),
            _ => {}
        }
    }

    let mut body = vec![0u8; length.min(MAX_BODY)];
    reader.read_exact(&mut body)?;
    let stream = reader.into_inner();

    // Compared in full and only after the body is drained, so an unauthorized
    // caller cannot learn anything from how quickly it was turned away.
    if presented.as_deref() != Some(token) {
        return Ok((stream, None));
    }
    Ok((stream, Some(Request { path, body })))
}

fn respond(stream: &mut TcpStream, code: u16, reason: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    let _ = stream.flush();
}
