//! `box-egress-v1` control and data frames.
//!
//! Control messages are WebSocket **text** frames containing a JSON object
//! with `"v": 1` and a `"type"` tag. Payload for a stream is a WebSocket
//! **binary** frame: 4-byte big-endian stream id, then raw bytes.

use serde::{Deserialize, Serialize};

/// Wire version in every control object (`v`).
pub const WIRE_VERSION: u8 = 1;

/// Protocol name used as `Sec-WebSocket-Protocol` and in status JSON.
pub const PROTOCOL_NAME: &str = "box-egress-v1";

/// How long the guest waits for the client's `opened` or `error` after an `open`.
pub const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Both ends ping an idle peer this often, and drop one silent for [`KEEPALIVE_MISSES`] intervals.
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub const KEEPALIVE_MISSES: u8 = 2;

/// Frame and message caps for both directions.
pub const MAX_FRAME: usize = 256 << 10;
pub const MAX_MESSAGE: usize = 1 << 20;
/// Bytes per data frame, at most.
pub const CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    Hello {
        v: u8,
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol: Option<String>,
    },
    Open {
        v: u8,
        id: u32,
        host: String,
        port: u16,
    },
    Opened {
        v: u8,
        id: u32,
    },
    Close {
        v: u8,
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Error {
        v: u8,
        id: u32,
        message: String,
    },
}

impl ControlMsg {
    pub fn hello(role: &str) -> Self {
        Self::Hello {
            v: WIRE_VERSION,
            role: role.to_string(),
            protocol: Some(PROTOCOL_NAME.to_string()),
        }
    }

    pub fn open(id: u32, host: String, port: u16) -> Self {
        Self::Open {
            v: WIRE_VERSION,
            id,
            host,
            port,
        }
    }

    pub fn opened(id: u32) -> Self {
        Self::Opened {
            v: WIRE_VERSION,
            id,
        }
    }

    pub fn close(id: u32, reason: Option<&str>) -> Self {
        Self::Close {
            v: WIRE_VERSION,
            id,
            reason: reason.map(str::to_string),
        }
    }

    pub fn error(id: u32, message: impl Into<String>) -> Self {
        Self::Error {
            v: WIRE_VERSION,
            id,
            message: message.into(),
        }
    }

    pub fn stream_id(&self) -> Option<u32> {
        match self {
            Self::Hello { .. } => None,
            Self::Open { id, .. }
            | Self::Opened { id, .. }
            | Self::Close { id, .. }
            | Self::Error { id, .. } => Some(*id),
        }
    }

    pub fn wire_ok(&self) -> bool {
        match self {
            Self::Hello { v, .. }
            | Self::Open { v, .. }
            | Self::Opened { v, .. }
            | Self::Close { v, .. }
            | Self::Error { v, .. } => *v == WIRE_VERSION,
        }
    }
}

/// Binary frame: `id` as u32 BE, then payload.
pub fn encode_data(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode_data(frame: &[u8]) -> Option<(u32, &[u8])> {
    if frame.len() < 4 {
        return None;
    }
    let id = u32::from_be_bytes(frame[0..4].try_into().ok()?);
    Some((id, &frame[4..]))
}

/// Parse `host:port`, `[ipv6]:port`, or `host` with `default_port`.
///
/// Port `0` is rejected. Unbracketed IPv6 (several colons, no port split) is
/// rejected so we never guess.
pub fn parse_hostport(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = authority[1..end].to_string();
        if host.is_empty() {
            return None;
        }
        let rest = &authority[end + 1..];
        if rest.is_empty() {
            return nonzero_port(host, default_port);
        }
        let port: u16 = rest.strip_prefix(':')?.parse().ok()?;
        return nonzero_port(host, port);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            let port: u16 = port.parse().ok()?;
            nonzero_port(host.to_string(), port)
        }
        None if !authority.contains(':') => nonzero_port(authority.to_string(), default_port),
        _ => None,
    }
}

fn nonzero_port(host: String, port: u16) -> Option<(String, u16)> {
    if port == 0 {
        None
    } else {
        Some((host, port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_open() {
        let msg = ControlMsg::open(7, "example.com".into(), 443);
        let raw = serde_json::to_string(&msg).unwrap();
        assert!(raw.contains("\"type\":\"open\""));
        assert!(raw.contains("\"v\":1"));
        let back: ControlMsg = serde_json::from_str(&raw).unwrap();
        assert_eq!(msg, back);
        assert!(back.wire_ok());
    }

    #[test]
    fn data_frame_roundtrip() {
        let frame = encode_data(42, b"abc");
        let (id, payload) = decode_data(&frame).unwrap();
        assert_eq!(id, 42);
        assert_eq!(payload, b"abc");
        assert!(decode_data(&[1, 2, 3]).is_none());
    }

    #[test]
    fn hostport_ipv4_and_name() {
        assert_eq!(
            parse_hostport("example.com:443", 0).unwrap(),
            ("example.com".into(), 443)
        );
        assert_eq!(
            parse_hostport("example.com", 80).unwrap(),
            ("example.com".into(), 80)
        );
        assert!(parse_hostport("example.com:0", 80).is_none());
        assert!(parse_hostport("", 80).is_none());
    }

    #[test]
    fn hostport_ipv6() {
        assert_eq!(parse_hostport("[::1]:443", 0).unwrap(), ("::1".into(), 443));
        assert_eq!(parse_hostport("[::1]", 80).unwrap(), ("::1".into(), 80));
        assert!(parse_hostport("::1", 80).is_none());
    }
}
