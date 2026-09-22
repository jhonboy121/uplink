//! Call signalling over one bidirectional QUIC stream: length-prefixed postcard frames.

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use serde::{Deserialize, Serialize};

use crate::Error;

pub const ALPN: &[u8] = b"uplink/call/0";
const MAX_SIGNAL_BYTES: usize = 1024;

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
}

type Length = u32;
const HEADER_BYTES: usize = size_of::<Length>();

/// Length header followed by the postcard body.
fn encode(signal: Signal) -> Result<Vec<u8>, Error> {
    let body = postcard::to_stdvec(&signal)?;
    let length = Length::try_from(body.len()).map_err(|_| Error::FrameTooLarge(body.len()))?;
    Ok([length.to_le_bytes().as_slice(), &body].concat())
}

fn frame_length(header: [u8; HEADER_BYTES]) -> Result<usize, Error> {
    let length = usize::try_from(Length::from_le_bytes(header)).map_err(|_| Error::FrameTooLarge(usize::MAX))?;
    if length > MAX_SIGNAL_BYTES {
        return Err(Error::FrameTooLarge(length));
    }
    Ok(length)
}

fn decode(body: &[u8]) -> Result<Signal, Error> {
    Ok(postcard::from_bytes(body)?)
}

pub async fn send(stream: &mut SendStream, signal: Signal) -> Result<(), Error> {
    stream.write_all(&encode(signal)?).await?;
    Ok(())
}

pub async fn recv(stream: &mut RecvStream) -> Result<Signal, Error> {
    let mut header = [0; HEADER_BYTES];
    stream.read_exact(&mut header).await?;
    let mut body = vec![0; frame_length(header)?];
    stream.read_exact(&mut body).await?;
    decode(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Signal; 5] = [Signal::Offer, Signal::Accept, Signal::Reject, Signal::Busy, Signal::Hangup];

    fn split(frame: &[u8]) -> Result<([u8; HEADER_BYTES], &[u8]), Error> {
        let (header, body) = frame.split_first_chunk().ok_or(Error::Protocol("short frame"))?;
        Ok((*header, body))
    }

    #[test]
    fn every_signal_round_trips() -> Result<(), Error> {
        for signal in ALL {
            let frame = encode(signal)?;
            let (header, body) = split(&frame)?;
            assert_eq!(frame_length(header)?, body.len());
            assert_eq!(decode(body)?, signal);
        }
        Ok(())
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let too_long = Length::try_from(MAX_SIGNAL_BYTES + 1).unwrap_or(Length::MAX);
        assert!(matches!(frame_length(too_long.to_le_bytes()), Err(Error::FrameTooLarge(_))));
    }

    #[test]
    fn garbage_does_not_decode() {
        let unknown_variant = [u8::MAX];
        assert!(decode(&unknown_variant).is_err());
        assert!(decode(&[]).is_err());
    }
}
