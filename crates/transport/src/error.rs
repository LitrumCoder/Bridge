use std::io;

use bridge_protocol::CloseCode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClosedBy {
    Local,
    Peer,
    /// Idle timeout, reset or another transport-level failure.
    Transport,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CloseInfo {
    pub by: ClosedBy,
    /// Application close code, when the connection was closed by the application.
    pub code: Option<CloseCode>,
    pub reason: String,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum TransportError {
    #[error("i/o error: {0}")]
    Io(String),
    #[error("tls error: {0}")]
    Tls(String),
    #[error("invalid device identity: {0}")]
    BadIdentity(&'static str),
    /// The peer refused our certificate during the handshake.
    #[error("the peer does not trust this device (not paired)")]
    NotPaired,
    /// The peer's certificate did not match the expected device.
    #[error("the peer presented an unexpected identity")]
    WrongPeer,
    #[error("timed out")]
    TimedOut,
    #[error("connection closed ({0:?})")]
    Closed(CloseInfo),
    #[error("{0}")]
    Other(String),
}

impl From<io::Error> for TransportError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
