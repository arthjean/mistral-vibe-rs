//! The HTTP/1.1 the platform and the sentinel speak: one request per
//! connection, answered with a length or as chunks, then closed.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// The largest body a request may carry, well above a 300 KB prompt.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// One parsed request.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub method: String,
    /// The request target as sent: a path with its query, or an absolute URI
    /// or an authority when a client talks to a proxy.
    pub target: String,
    /// Header names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The target without its query string.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }
}

/// Reads the request line and headers, then the body by length or by chunks.
/// [`None`] when the peer closed before sending a request line.
pub(crate) fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed request line {line:?}"),
        ));
    };
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let mut request = Request {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    if request
        .header("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        request.body = read_chunked(reader)?;
    } else if let Some(length) = request.header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad content-length"))?;
        if length > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        request.body = body;
    }
    Ok(Some(request))
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            line.clear();
            reader.read_line(&mut line)?;
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?;
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        425 => "Too Early",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn head(status: u16, headers: &[(String, String)]) -> String {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nconnection: close\r\n",
        reason(status)
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head
}

/// Writes a whole response with its length.
pub(crate) fn respond(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = head(status, headers);
    head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// A response sent as chunks, each one flushed in its own write.
pub(crate) struct Chunked<'s> {
    stream: &'s mut TcpStream,
}

impl<'s> Chunked<'s> {
    pub fn begin(
        stream: &'s mut TcpStream,
        status: u16,
        headers: &[(String, String)],
    ) -> io::Result<Self> {
        let mut head = head(status, headers);
        head.push_str("transfer-encoding: chunked\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        stream.flush()?;
        Ok(Self { stream })
    }

    pub fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut frame = format!("{:x}\r\n", bytes.len()).into_bytes();
        frame.extend_from_slice(bytes);
        frame.extend_from_slice(b"\r\n");
        self.stream.write_all(&frame)?;
        self.stream.flush()
    }

    /// Ends the body cleanly with the terminating chunk.
    pub fn finish(self) -> io::Result<()> {
        self.stream.write_all(b"0\r\n\r\n")?;
        self.stream.flush()
    }

    /// Closes the connection without the terminating chunk, as a connection
    /// that drops mid-stream does.
    pub fn drop_connection(self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}
