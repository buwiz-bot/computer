//! A GET to a peer on the box's own loopback (another daemon's health or status), with a deadline
//! on the whole exchange and a cap on what is read. A peer that hangs, trickles, or never stops
//! talking costs the caller at most `deadline` and `cap` bytes.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::http_response_complete;

/// A peer's answer: its status code and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// `GET path` on `addr` (`host:port`), HTTP/1.0, so the answer comes with a length or ends with
/// the connection. Connect, send and read all happen within `deadline`, or it is
/// [`io::ErrorKind::TimedOut`]. An answer longer than `cap` bytes, headers included, is
/// [`io::ErrorKind::InvalidData`], as is one that isn't HTTP.
pub async fn local_get(
    addr: &str,
    path: &str,
    deadline: Duration,
    cap: usize,
) -> io::Result<LocalResponse> {
    let exchange = async {
        let mut stream = TcpStream::connect(addr).await?;
        let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            if buf.len() + n > cap {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("answer from {addr} is longer than {cap} bytes"),
                ));
            }
            buf.extend_from_slice(&chunk[..n]);
            // A server that keeps the socket open after a whole answer is done talking.
            if http_response_complete(&buf) {
                break;
            }
        }
        parse(&buf).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("answer from {addr} is not HTTP"),
            )
        })
    };
    tokio::time::timeout(deadline, exchange)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{addr} did not answer within {deadline:?}"),
            )
        })?
}

fn parse(buf: &[u8]) -> Option<LocalResponse> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let status_line = head.lines().next()?;
    let mut words = status_line.split_whitespace();
    if !words.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let status = words.next()?.parse().ok()?;
    let mut body = buf[end + 4..].to_vec();
    let length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    });
    if let Some(length) = length {
        body.truncate(length);
    }
    Some(LocalResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A peer that reads the request, then writes `answer` and waits for `linger`.
    async fn peer(answer: Vec<u8>, linger: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(&answer).await;
            tokio::time::sleep(linger).await;
        });
        addr
    }

    #[tokio::test]
    async fn reads_a_whole_answer_even_when_the_socket_stays_open() {
        let addr = peer(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            Duration::from_secs(30),
        )
        .await;
        let got = local_get(&addr, "/v1/health", Duration::from_secs(5), 1024)
            .await
            .unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"{}");
    }

    #[tokio::test]
    async fn a_silent_peer_times_out() {
        let addr = peer(Vec::new(), Duration::from_secs(30)).await;
        let err = local_get(&addr, "/", Duration::from_millis(100), 1024)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_peer_that_never_stops_is_cut_at_the_cap() {
        let mut answer = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        answer.extend(std::iter::repeat_n(b'x', 64 * 1024));
        let addr = peer(answer, Duration::from_secs(30)).await;
        let err = local_get(&addr, "/", Duration::from_secs(5), 1024)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn an_answer_ended_by_close_is_read_to_its_end() {
        let addr = peer(
            b"HTTP/1.0 503 Service Unavailable\r\n\r\nbusy".to_vec(),
            Duration::ZERO,
        )
        .await;
        let got = local_get(&addr, "/", Duration::from_secs(5), 1024)
            .await
            .unwrap();
        assert_eq!(got.status, 503);
        assert_eq!(got.body, b"busy");
    }
}
