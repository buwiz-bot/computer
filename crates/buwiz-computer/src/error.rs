use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Connect(String),
    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        body: Value,
    },
    #[error("{0}")]
    Transport(String),
}

impl Error {
    pub fn from_status(status: u16, text: &str) -> Self {
        let body: Value = serde_json::from_str(text).unwrap_or_else(|_| json_raw(text));
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or(text)
            .to_string();
        Self::Http {
            status,
            message,
            body,
        }
    }
}

fn json_raw(text: &str) -> Value {
    Value::Object({
        let mut map = serde_json::Map::new();
        map.insert("raw".into(), Value::String(text.to_string()));
        map
    })
}
