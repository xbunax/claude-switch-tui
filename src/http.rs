// ---------------------------------------------------------------------------
// Minimal blocking HTTP server primitives.
//
// Shared by the OAuth callback server (src/oauth.rs) and the translation
// proxy daemon (src/proxy.rs). Handles exactly what those two need:
// Content-Length request bodies, plain JSON responses, and SSE streams.
// ---------------------------------------------------------------------------

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 8192;

/// One parsed HTTP/1.1 request (Content-Length bodies only).
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Query parameters from the request path (`?a=b&c=d`).
    pub fn query(&self) -> Vec<(String, String)> {
        self.path
            .split_once('?')
            .map(|(_, q)| {
                q.split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Path without the query string.
    pub fn path_no_query(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

/// Read one request from the stream. `deadline` caps the total blocking time.
pub fn read_request(stream: &mut TcpStream, deadline: Duration) -> anyhow::Result<HttpRequest> {
    stream.set_read_timeout(Some(deadline))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || path.is_empty() {
        anyhow::bail!("malformed request line");
    }

    let mut content_length: Option<String> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        header_bytes += trimmed.len();
        if header_bytes > MAX_HEADER_BYTES {
            anyhow::bail!("request headers too large");
        }
        let (name, value) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("malformed header line"))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().to_string());
        }
    }

    let mut body = Vec::new();
    if let Some(len) = content_length {
        let len: usize = len
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid content-length"))?;
        body.resize(len, 0);
        reader.read_exact(&mut body)?;
    }

    // Clear the deadline so long-lived response streams are unaffected.
    stream.set_read_timeout(None)?;

    Ok(HttpRequest {
        method,
        path,
        body,
    })
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        529 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Write a plain response with a Content-Length body (Connection: close).
pub fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text(status),
        content_type,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Write a response without a body (for HEAD requests).
pub fn write_response_empty(stream: &mut TcpStream, status: u16) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status,
        status_text(status)
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// Write a JSON response.
pub fn write_json(stream: &mut TcpStream, status: u16, body: &serde_json::Value) -> io::Result<()> {
    write_response(stream, status, "application/json", body.to_string().as_bytes())
}

/// Write an error body in the Anthropic shape `{"type":"error","error":{...}}`.
pub fn write_error(stream: &mut TcpStream, status: u16, error_type: &str, message: &str) -> io::Result<()> {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": error_type, "message": message },
    });
    write_json(stream, status, &body)
}

/// Start an SSE response. The caller then writes framed events and closes.
pub fn write_sse_headers(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    stream.flush()
}

/// Frames `event: <name>` + `data: <json>` blocks, flushing after each event.
pub struct SseWriter<W: Write> {
    inner: W,
}

impl<W: Write> SseWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn event(&mut self, name: &str, data: &str) -> io::Result<()> {
        self.inner
            .write_all(format!("event: {}\ndata: {}\n\n", name, data).as_bytes())?;
        self.inner.flush()
    }

    /// Keep-alive event so Claude Code's silent-stream watchdog stays happy.
    pub fn ping(&mut self) -> io::Result<()> {
        self.inner
            .write_all(b"event: ping\ndata: {\"type\":\"ping\"}\n\n")?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn test_read_request_parses_query_and_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(
                b"POST /auth/callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\npayload",
            )
            .unwrap();
            // keep the socket open briefly so the server can respond
            std::thread::sleep(Duration::from_millis(200));
        });
        let (mut stream, _) = listener.accept().unwrap();
        let req = read_request(&mut stream, Duration::from_secs(2)).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path_no_query(), "/auth/callback");
        assert_eq!(req.body, b"payload");
        let q = req.query();
        assert!(q.contains(&("code".into(), "abc".into())));
        assert!(q.contains(&("state".into(), "xyz".into())));
        client.join().unwrap();
    }

    #[test]
    fn test_sse_writer_framing() {
        let mut w = SseWriter::new(Vec::<u8>::new());
        w.event("message_start", "{\"type\":\"message_start\"}").unwrap();
        w.ping().unwrap();
        let out = String::from_utf8(w.inner).unwrap();
        assert_eq!(
            out,
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
    }
}
