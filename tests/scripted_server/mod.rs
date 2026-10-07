//! A scripted HTTP/1.1 server for the tests wiremock cannot express: a
//! stream written frame by frame and held open, a connection dropped
//! before any response, a handshake that answers late, and noticing when
//! the client lets a connection go.
//!
//! Plain `std::net` on threads, so it needs no async runtime of its own
//! and no dependency beyond the standard library. Every response carries
//! `Connection: close`, so each request arrives on a fresh connection and
//! the accepted-request count is exactly the number of attempts.
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// One request as it arrived.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The server's side of one exchange.
pub struct Exchange {
    socket: TcpStream,
    pub request: Request,
    /// Zero-based order in which requests arrived.
    pub index: usize,
}

impl Exchange {
    /// Answer with a whole body.
    pub fn respond(&mut self, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ));
        let _ = self.socket.write_all(head.as_bytes());
        let _ = self.socket.write_all(body);
        let _ = self.socket.flush();
    }

    /// Promise `declared` bytes of body, send `body`, and close: a response
    /// that broke off after it began.
    pub fn respond_partial(&mut self, status: u16, body: &[u8], declared: usize) {
        let head = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n",
            reason(status)
        );
        let _ = self.socket.write_all(head.as_bytes());
        let _ = self.socket.write_all(body);
        let _ = self.socket.flush();
    }

    /// Answer 200 with an event stream whose body runs until the connection
    /// closes.
    pub fn open_stream(&mut self) {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    X-Request-Id: request_01stream\r\nConnection: close\r\n\r\n";
        let _ = self.socket.write_all(head.as_bytes());
        let _ = self.socket.flush();
    }

    pub fn frame(&mut self, event: &str, data: &str) {
        let _ = self
            .socket
            .write_all(format!("event: {event}\ndata: {data}\n\n").as_bytes());
        let _ = self.socket.flush();
    }

    pub fn open(&mut self, cursor: &str) {
        self.frame("open", &format!(r#"{{"cursor":"{cursor}"}}"#));
    }

    pub fn keepalive(&mut self) {
        let _ = self.socket.write_all(b": keepalive\n\n");
        let _ = self.socket.flush();
    }

    /// Keep the connection open until the client leaves or `max` passes.
    /// True when the client left. The bound only stops a broken test
    /// hanging the run.
    pub fn hold(&mut self, max: Duration) -> bool {
        let deadline = Instant::now() + max;
        let mut buf = [0u8; 256];
        while Instant::now() < deadline {
            let _ = self
                .socket
                .set_read_timeout(Some(Duration::from_millis(50)));
            match self.socket.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return true,
            }
        }
        false
    }
}

/// A running server. It lives for the rest of the test process.
pub struct ScriptedServer {
    pub url: String,
    accepted: Arc<AtomicUsize>,
}

impl ScriptedServer {
    /// Start a server that runs `script` once per request, each on its own
    /// thread, and closes the connection when the script returns.
    pub fn start<F>(script: F) -> Self
    where
        F: Fn(Exchange) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let accepted = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let script = Arc::clone(&script);
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    let Some(request) = read_request(&mut socket) else {
                        return;
                    };
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    script(Exchange {
                        socket,
                        request,
                        index,
                    });
                });
            }
        });
        Self { url, accepted }
    }

    /// How many requests have arrived.
    pub fn requests(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn read_request(socket: &mut TcpStream) -> Option<Request> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = raw[head_end..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        403 => "Forbidden",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
