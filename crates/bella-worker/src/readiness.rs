//! Loopback readiness probes for service steps.
//!
//! These are the only sockets this crate opens, and they only ever connect
//! to the loopback addresses of the machine the service runs on.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

/// Upper bound for one connect, write or read of a probe.
const IO_TIMEOUT: Duration = Duration::from_millis(1000);
/// Bytes of an HTTP response read to find the status line.
const STATUS_LINE_LIMIT: usize = 1024;

fn connect(port: u16) -> Option<TcpStream> {
    let addresses = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, IO_TIMEOUT).ok())
}

/// Does a TCP connection to the loopback `port` succeed right now?
pub(crate) fn tcp_ready(port: u16) -> bool {
    connect(port).is_some()
}

/// Does an HTTP GET of `path` on the loopback `port` answer with `status`
/// exactly, or without one with any status below 500?
pub(crate) fn http_ready(port: u16, path: &str, status: Option<u16>) -> bool {
    let Some(code) = http_status(port, path) else {
        return false;
    };
    match status {
        Some(expected) => code == expected,
        None => code < 500,
    }
}

fn http_status(port: u16, path: &str) -> Option<u16> {
    let mut stream = connect(port)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let path = if path.is_empty() { "/" } else { path };
    // A request line cannot carry control characters; a path with them
    // never matches anything.
    if path.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return None;
    }
    let separator = if path.starts_with('/') { "" } else { "/" };
    let request = format!(
        "GET {separator}{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: builderella-readiness\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut head = Vec::new();
    let mut buffer = [0u8; 256];
    while head.len() < STATUS_LINE_LIMIT && !head.contains(&b'\n') {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    parse_status_line(&head)
}

/// The status code of an HTTP/1.x status line, e.g. `HTTP/1.1 204 No Content`.
fn parse_status_line(head: &[u8]) -> Option<u16> {
    let line = head.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end();
    let mut parts = line.split_ascii_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 {
        return None;
    }
    code.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn status_lines_are_parsed_strictly() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status_line(b"HTTP/1.0 503 Busy\r\nx: y\r\n"), Some(503));
        assert_eq!(parse_status_line(b"HTTP/1.1 204\r\n"), Some(204));
        assert_eq!(parse_status_line(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 20 OK\r\n"), None);
        assert_eq!(parse_status_line(b""), None);
    }

    fn serve_once(response: &'static str) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 512];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(response.as_bytes());
        });
        (port, handle)
    }

    #[test]
    fn http_probe_applies_the_status_rule() {
        let (port, server) = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        assert!(http_ready(port, "/health", None));
        server.join().unwrap();

        let (port, server) = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        assert!(!http_ready(port, "/health", Some(200)));
        server.join().unwrap();

        let (port, server) = serve_once("HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n");
        assert!(!http_ready(port, "", None));
        server.join().unwrap();
    }

    #[test]
    fn tcp_probe_sees_a_listener_and_its_absence() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp_ready(port));
        drop(listener);
        assert!(!tcp_ready(port));
        assert!(!http_ready(port, "/", None));
    }
}
