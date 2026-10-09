//! 采集器错误类型（docs/02）。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("config io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("config json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("connector(venue={venue}): {msg}")]
    Connector { venue: String, msg: String },

    #[error("subscribe(venue={venue}): {msg}")]
    Subscribe { venue: String, msg: String },

    #[error("publish(topic={topic}): {msg}")]
    Publish { topic: String, msg: String },

    #[error("envelope: {0}")]
    Envelope(String),

    #[error("no event center backend available: {0}")]
    EventCenter(String),
}

impl Error {
    pub fn connector(venue: impl Into<String>, msg: impl std::fmt::Display) -> Self {
        Error::Connector {
            venue: venue.into(),
            msg: msg.to_string(),
        }
    }

    pub fn subscribe(venue: impl Into<String>, msg: impl std::fmt::Display) -> Self {
        Error::Subscribe {
            venue: venue.into(),
            msg: msg.to_string(),
        }
    }
}
