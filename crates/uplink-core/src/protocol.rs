//! Wire format: length-prefixed postcard messages. Call signalling runs over one bidirectional
//! stream; video frame headers use the same encoding at the start of each unidirectional stream.

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;

pub const ALPN: &[u8] = b"uplink/call/0";
/// Upper bound for any single message (signals and frame headers are a few bytes).
const MAX_MESSAGE_BYTES: usize = 1024;

pub const CLOSE_HANGUP: VarInt = VarInt::from_u32(0);
pub const CLOSE_REJECTED: VarInt = VarInt::from_u32(1);
pub const CLOSE_BUSY: VarInt = VarInt::from_u32(2);
pub const CLOSE_PROTOCOL: VarInt = VarInt::from_u32(3);
pub const CLOSE_NOT_POST_QUANTUM: VarInt = VarInt::from_u32(4);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Signal {
    Offer,
    Accept,
    Reject,
    Busy,
    Hangup,
    /// The receiver lost a reference frame and needs a new keyframe.
    KeyframeRequest,
}

type Length = u32;
const HEADER_BYTES: usize = size_of::<Length>();

/// Length header followed by the postcard body.
pub(crate) fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, Error> {
    let body = postcard::to_stdvec(message)?;
    let length = Length::try_from(body.len()).map_err(|_| Error::FrameTooLarge(body.len()))?;
    Ok([length.to_le_bytes().as_slice(), &body].concat())
}

fn message_length(header: [u8; HEADER_BYTES]) -> Result<usize, Error> {
    let length = usize::try_from(Length::from_le_bytes(header)).map_err(|_| Error::FrameTooLarge(usize::MAX))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(Error::FrameTooLarge(length));
    }
    Ok(length)
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    Ok(postcard::from_bytes(body)?)
}

/// Splits an [`encode`]d message off the front of `bytes`; returns it and the rest.
pub(crate) fn split_message<T: DeserializeOwned>(bytes: &[u8]) -> Result<(T, &[u8]), Error> {
    let (header, rest) = bytes.split_first_chunk().ok_or(Error::Protocol("short message"))?;
    let (body, rest) = rest.split_at_checked(message_length(*header)?).ok_or(Error::Protocol("short message"))?;
    Ok((decode(body)?, rest))
}

pub async fn write_message<T: Serialize>(stream: &mut SendStream, message: &T) -> Result<(), Error> {
    stream.write_all(&encode(message)?).await?;
    Ok(())
}

pub async fn read_message<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T, Error> {
    let mut header = [0; HEADER_BYTES];
    stream.read_exact(&mut header).await?;
    let mut body = vec![0; message_length(header)?];
    stream.read_exact(&mut body).await?;
    decode(&body)
}

pub async fn send(stream: &mut SendStream, signal: Signal) -> Result<(), Error> {
    write_message(stream, &signal).await
}

pub async fn recv(stream: &mut RecvStream) -> Result<Signal, Error> {
    read_message(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Signal; 6] =
        [Signal::Offer, Signal::Accept, Signal::Reject, Signal::Busy, Signal::Hangup, Signal::KeyframeRequest];

    fn split(frame: &[u8]) -> Result<([u8; HEADER_BYTES], &[u8]), Error> {
        let (header, body) = frame.split_first_chunk().ok_or(Error::Protocol("short frame"))?;
        Ok((*header, body))
    }

    #[test]
    fn every_signal_round_trips() -> Result<(), Error> {
        for signal in ALL {
            let frame = encode(&signal)?;
            let (header, body) = split(&frame)?;
            assert_eq!(message_length(header)?, body.len());
            assert_eq!(decode::<Signal>(body)?, signal);
        }
        Ok(())
    }

    #[test]
    fn oversized_messages_are_rejected() {
        let too_long = Length::try_from(MAX_MESSAGE_BYTES + 1).unwrap_or(Length::MAX);
        assert!(matches!(message_length(too_long.to_le_bytes()), Err(Error::FrameTooLarge(_))));
    }

    #[test]
    fn garbage_does_not_decode() {
        let unknown_variant = [u8::MAX];
        assert!(decode::<Signal>(&unknown_variant).is_err());
        assert!(decode::<Signal>(&[]).is_err());
    }
}
