//! The egress sentinel: a loopback proxy that refuses every request and
//! remembers the host each one was for.
//!
//! A world exports it as `HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY` with
//! loopback exempted, so a binary that reaches for a real host reaches the
//! sentinel instead, and the trial fails naming that host.

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use crate::http;

#[derive(Default)]
struct Shared {
    hosts: Mutex<Vec<String>>,
    stop: AtomicBool,
}

pub(crate) struct Sentinel {
    base: String,
    address: std::net::SocketAddr,
    shared: Arc<Shared>,
}

impl Sentinel {
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared::default());
        let accepting = Arc::clone(&shared);
        thread::Builder::new()
            .name("e2e-sentinel".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accepting.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let shared = Arc::clone(&accepting);
                    let _ = thread::Builder::new()
                        .name("e2e-sentinel-connection".to_owned())
                        .spawn(move || refuse(stream, &shared));
                }
            })?;
        Ok(Self {
            base: format!("http://{address}"),
            address,
            shared,
        })
    }

    /// The proxy URL a world exports.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Every `host:port` a request was for, in arrival order.
    pub fn hosts(&self) -> Vec<String> {
        self.shared
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Forgets what was recorded, for a self-test that provoked egress on
    /// purpose and checked it.
    pub fn clear(&self) {
        self.shared
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
    }
}

fn refuse(stream: TcpStream, shared: &Shared) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let Ok(Some(request)) = http::read_request(&mut reader) else {
        return;
    };
    if let Some(host) = target_host(&request.method, &request.target) {
        shared
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host);
    }
    let mut stream = stream;
    let _ = http::respond(&mut stream, 403, &[], b"");
    let _ = stream.flush();
}

/// The `host:port` a proxy request names: the authority of a `CONNECT`, or
/// the authority of an absolute URI with the scheme's default port.
pub(crate) fn target_host(method: &str, target: &str) -> Option<String> {
    if method.eq_ignore_ascii_case("CONNECT") {
        return Some(target.to_owned());
    }
    let (scheme, rest) = target.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    if authority.contains(':') {
        return Some(authority.to_owned());
    }
    let port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    Some(format!("{authority}:{port}"))
}

#[cfg(test)]
mod sentinel_tests;
