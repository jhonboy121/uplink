use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}: not a valid secret key file")]
    CorruptKey(PathBuf),
    #[error(transparent)]
    KeyParse(#[from] iroh::KeyParsingError),
    #[error("contacts: {0}")]
    Contacts(#[from] rusqlite::Error),
    #[error("contact `{0}` already exists")]
    DuplicateContact(String),
    #[error("unknown contact `{0}`")]
    UnknownContact(String),
    #[error(transparent)]
    Bind(#[from] iroh::endpoint::BindError),
    #[error(transparent)]
    Connect(#[from] iroh::endpoint::ConnectError),
    #[error(transparent)]
    Connecting(#[from] iroh::endpoint::ConnectingError),
    #[error(transparent)]
    Connection(#[from] iroh::endpoint::ConnectionError),
    #[error(transparent)]
    Write(#[from] iroh::endpoint::WriteError),
    #[error(transparent)]
    Read(#[from] iroh::endpoint::ReadExactError),
    #[error(transparent)]
    Encoding(#[from] postcard::Error),
    #[error("signal frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("unexpected signal: {0}")]
    Protocol(&'static str),
    #[error("peer connection is not post-quantum (key exchange {0:?})")]
    NotPostQuantum(Option<rustls::NamedGroup>),
    #[error("frame missed its delivery deadline")]
    FrameLate,
    #[error(transparent)]
    ReadToEnd(#[from] iroh::endpoint::ReadToEndError),
    #[error(transparent)]
    ClosedStream(#[from] iroh::endpoint::ClosedStream),
    #[error("opus: {0}")]
    Opus(String),
    #[error("qr: {0}")]
    Qr(String),
    #[error("node command queue is full")]
    CommandQueueFull,
    #[error("node stopped")]
    NodeStopped,
}
