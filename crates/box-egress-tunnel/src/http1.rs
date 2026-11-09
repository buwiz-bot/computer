//! Tiny HTTP/1.1 request-head reader for the CONNECT proxy.

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::protocol::parse_hostport;

const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct ProxyRequest {
    pub method: String,
    pub host: String,
    pub port: u16,
    /// CONNECT vs origin/absolute-form HTTP.
    pub connect: bool,
    /// Bytes to send into the tunnel after the remote TCP is up.
    /// CONNECT: leftover after the header (usually empty).
    /// HTTP: rewritten request head plus any already-read body bytes.
    pub initial: Vec<u8>,
}

pub fn split_head(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let pos = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    Some((&buf[..pos + 4], &buf[pos + 4..]))
}

pub async fn read_http_head(stream: &mut TcpStream) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP header too large",
            ));
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some((head, rest)) = split_head(&buf) {
            return Ok((head.to_vec(), rest.to_vec()));
        }
    }
}

pub fn parse_proxy_request(head: &[u8], rest: &[u8]) -> Option<ProxyRequest> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let _version = parts.next()?;

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = parse_hostport(&target, 0)?;
        return Some(ProxyRequest {
            method,
            host,
            port,
            connect: true,
            initial: rest.to_vec(),
        });
    }

    let host_header = header_value(text, "host");
    let (host, port, origin_target) = if let Some(parsed) = parse_absolute_http(&target) {
        parsed
    } else {
        let host_header = host_header?;
        let (host, port) = parse_hostport(host_header, 80)?;
        (host, port, target)
    };

    let rewritten = rewrite_origin_form(head, &method, &origin_target)?;
    let mut initial = rewritten;
    initial.extend_from_slice(rest);
    Some(ProxyRequest {
        method,
        host,
        port,
        connect: false,
        initial,
    })
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.split("\r\n").skip(1) {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim());
        }
    }
    None
}

fn parse_absolute_http(target: &str) -> Option<(String, u16, String)> {
    let (rest, default_port) = match target.strip_prefix("http://") {
        Some(rest) => (rest, 80u16),
        None => (target.strip_prefix("https://")?, 443u16),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = parse_hostport(authority, default_port)?;
    Some((host, port, path.to_string()))
}

fn rewrite_origin_form(head: &[u8], method: &str, origin_target: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let version = first.split_whitespace().nth(2)?;
    let mut out = format!("{method} {origin_target} {version}\r\n").into_bytes();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, _)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("proxy-connection") {
                continue;
            }
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connect() {
        let head = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
        let req = parse_proxy_request(head, b"leftover").unwrap();
        assert!(req.connect);
        assert_eq!(req.host, "example.com");
        assert_eq!(req.port, 443);
        assert_eq!(req.initial, b"leftover");
    }

    #[test]
    fn parse_absolute_get() {
        let head = b"GET http://example.com/foo HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\n\r\n";
        let req = parse_proxy_request(head, b"").unwrap();
        assert!(!req.connect);
        assert_eq!(req.host, "example.com");
        assert_eq!(req.port, 80);
        let text = String::from_utf8(req.initial).unwrap();
        assert!(text.starts_with("GET /foo HTTP/1.1\r\n"));
        assert!(!text.to_ascii_lowercase().contains("proxy-connection"));
    }
}
