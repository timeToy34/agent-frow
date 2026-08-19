//! One blocking HTTP POST to the app, hand-written.
//!
//! The only server this ever talks to is our own, on loopback, and it always
//! sends `content-length` and `connection: close`. That makes a real HTTP client
//! dependency pure weight in a binary whose job is to start, say twenty short
//! words, and stop.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

/// Long enough for a loopback round trip on a busy machine, short enough that a
/// dead app never becomes the agent's problem. Every hook we register is either
/// non-blocking or marked async, but Codex blocks by default and the cost of
/// being wrong about that is an agent that appears to hang.
///
/// One second, not the original 300 ms: measured against a real day's traffic,
/// the short cap lost dozens of posts whenever the app was busy for an instant
/// (`hook.log` was nothing but "post failed (timed out)"). The agents give
/// hooks a 5 s budget, so a second of patience is still comfortably fast.
const TIMEOUT: Duration = Duration::from_millis(1000);

pub fn send(addr: SocketAddr, token: &str, body: &[u8]) -> Result<(), std::io::Error> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.set_read_timeout(Some(TIMEOUT))?;

    let head = format!(
        "POST /hook HTTP/1.1\r\n\
         host: {addr}\r\n\
         content-type: application/json\r\n\
         x-agent-frow-token: {token}\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    // Read and discard the reply. We do not care what it says — there is no
    // decision to relay and never will be — but leaving the response unread can
    // reset the connection before the server has finished writing it, which
    // shows up on the far side as a spurious error.
    let mut sink = [0u8; 256];
    let _ = stream.read(&mut sink);
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}
