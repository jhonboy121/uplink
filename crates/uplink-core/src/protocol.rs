//! The wire: protobuf messages, each behind a varint length (standard length-delimited protobuf),
//! over one QUIC connection per call. Rules and the tag registry are in `docs/ref/wire.md`.
//!
//! - **Signalling** is one bidirectional stream of [`Signal`]s. The first each side sends — the
//!   offer and its answer — carries a [`Hello`]: protocol, app version, capabilities.
//! - **Video** is one unidirectional stream per frame: a [`StreamHeader`], then the frame's bytes
//!   to the end of the stream.
//! - **Voice** is one datagram per packet: a [`DatagramHeader`], then the Opus packet.
//!
//! Protobuf for its evolution rules: every field is tagged, so an older build skips fields it does
//! not know and a newer one reads a missing field as its default; a `oneof` kind an older build
//! does not know decodes as none, which it ignores rather than calling it a protocol error. The
//! messages are derived Rust structs — no `.proto`, no `protoc`.

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use prost::{Enumeration, Message, Oneof};

use crate::Error;

/// Changed only for a break these rules cannot absorb; accepting old and new side by side for a
/// while is then how both kinds of build keep calling each other.
pub const ALPN: &[u8] = b"uplink/1";
/// This build's protocol, for logs and for people reading them. Compatibility is decided by
/// capabilities, never by comparing this.
pub const PROTOCOL: u32 = 1;
/// Upper bound for any single message: signals and headers are a few dozen bytes.
const MAX_MESSAGE_BYTES: usize = 1024;
/// A varint holding a `u32` never needs more than this.
const MAX_LENGTH_BYTES: u32 = 5;
const VARINT_MORE: u8 = 0x80;
const VARINT_BITS: u8 = 0x7f;
const VARINT_SHIFT: u32 = 7;

pub const CLOSE_HANGUP: VarInt = VarInt::from_u32(0);
pub const CLOSE_REJECTED: VarInt = VarInt::from_u32(1);
pub const CLOSE_BUSY: VarInt = VarInt::from_u32(2);
pub const CLOSE_PROTOCOL: VarInt = VarInt::from_u32(3);
pub const CLOSE_NOT_POST_QUANTUM: VarInt = VarInt::from_u32(4);
/// One side requires a capability the other does not have.
pub const CLOSE_INCOMPATIBLE: VarInt = VarInt::from_u32(5);
/// The call carried on over a newer connection, and this one is no longer needed.
pub const CLOSE_REJOINED: VarInt = VarInt::from_u32(6);

/// Something a build can take part in. A new one takes the next number; a number is never reused,
/// not even once its feature is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Capability {
    Unspecified = 0,
    /// H.264 frames, one per unidirectional stream, and keyframe requests.
    Video = 1,
    /// Opus packets, one per datagram.
    Voice = 2,
}

/// What this build can do.
const SUPPORTED: [Capability; 2] = [Capability::Video, Capability::Voice];
/// What this build cannot call without. Nothing yet: the first feature an older build would
/// break goes here, and from then on that build is told to update rather than failing mid-call.
const REQUIRED: [Capability; 0] = [];

/// What each side can do and needs, carried by the offer and by its answer.
#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub protocol: u32,
    /// The app's own version, for people: it is what the "update" notice names on either side.
    #[prost(string, tag = "2")]
    pub app: String,
    /// Kept as numbers, so a capability this build has never heard of is still counted.
    #[prost(enumeration = "Capability", repeated, tag = "3")]
    pub supports: Vec<i32>,
    #[prost(enumeration = "Capability", repeated, tag = "4")]
    pub requires: Vec<i32>,
    /// The call an offer is for; absent from answers.
    #[prost(message, optional, tag = "5")]
    pub setup: Option<Setup>,
}

/// One call, as its offer describes it.
#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct Setup {
    /// Names the call, so a re-dial after a drop can say which call it rejoins.
    #[prost(uint64, tag = "1")]
    pub call: u64,
    /// Placed as voice alone: no camera, and no video streams.
    #[prost(bool, tag = "2")]
    pub voice: bool,
    /// Rejoins `call` after its connection dropped, rather than ringing.
    #[prost(bool, tag = "3")]
    pub resume: bool,
}

impl Hello {
    pub fn ours(app: &str) -> Self {
        let numbers = |capabilities: &[Capability]| capabilities.iter().map(|&c| i32::from(c)).collect();
        Self {
            protocol: PROTOCOL,
            app: app.to_owned(),
            supports: numbers(&SUPPORTED),
            requires: numbers(&REQUIRED),
            setup: None,
        }
    }

    /// This build's hello as the offer for `setup`.
    pub fn offer(&self, setup: Setup) -> Self {
        Self { setup: Some(setup), ..self.clone() }
    }

    /// Whether `other` needs something this side cannot do.
    fn lacks_what(&self, other: &Self) -> bool {
        other.requires.iter().any(|needed| !self.supports.contains(needed))
    }
}

/// Which side is too old for the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behind {
    Us,
    Them,
}

/// `None` when the two can call each other.
pub fn behind(ours: &Hello, theirs: &Hello) -> Option<Behind> {
    if ours.lacks_what(theirs) {
        Some(Behind::Us)
    } else if theirs.lacks_what(ours) {
        Some(Behind::Them)
    } else {
        None
    }
}

/// A signal that carries nothing but its kind.
#[derive(Clone, Copy, PartialEq, Message)]
pub struct Empty {}

/// Whether each side's mic and camera are on, whether its phone has put the call on hold, and
/// whether its screen can be captured, sent whenever any changes. The defaults are what a build
/// that never sends it has: both on, not held, nothing asked, capturable.
#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct MediaState {
    #[prost(bool, tag = "1")]
    pub mic_off: bool,
    #[prost(bool, tag = "2")]
    pub camera_off: bool,
    /// A phone call was answered over this one: nothing is sent or played until it ends.
    #[prost(bool, tag = "3")]
    pub held: bool,
    /// Asks the other phone to keep its screen from screenshots and recordings for this call.
    /// A build that does not know it cannot, and says nothing back.
    #[prost(bool, tag = "4")]
    pub capture_asked: bool,
    /// This phone's screen cannot be captured now: its own choice, or the other side's ask.
    #[prost(bool, tag = "5")]
    pub capture_blocked: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct VideoAsk {
    /// The asker changed their mind before it was answered.
    #[prost(bool, tag = "1")]
    withdrawn: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct VideoAnswer {
    #[prost(bool, tag = "1")]
    accepted: bool,
}

#[derive(Clone, PartialEq, Message)]
struct WireSignal {
    #[prost(oneof = "SignalKind", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10")]
    kind: Option<SignalKind>,
}

#[derive(Clone, PartialEq, Oneof)]
enum SignalKind {
    #[prost(message, tag = "1")]
    Offer(Hello),
    #[prost(message, tag = "2")]
    Accept(Hello),
    #[prost(message, tag = "3")]
    Reject(Empty),
    #[prost(message, tag = "4")]
    Busy(Empty),
    #[prost(message, tag = "5")]
    Hangup(Empty),
    #[prost(message, tag = "6")]
    KeyframeRequest(Empty),
    /// Instead of ringing, or instead of carrying on after an answer: the two cannot call.
    #[prost(message, tag = "7")]
    Incompatible(Hello),
    /// An older build ignores it, and shows a muted mic or a stopped camera as it always has.
    #[prost(message, tag = "8")]
    Media(MediaState),
    #[prost(message, tag = "9")]
    VideoAsk(VideoAsk),
    #[prost(message, tag = "10")]
    VideoAnswer(VideoAnswer),
}

/// One message on the signalling stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Signal {
    Offer(Hello),
    Accept(Hello),
    Reject,
    Busy,
    Hangup,
    /// The receiver lost a reference frame and needs a new keyframe.
    KeyframeRequest,
    Incompatible(Hello),
    /// The sender's mic or camera changed.
    Media(MediaState),
    /// Asks to switch a voice call to video.
    AskVideo,
    /// Takes back an ask not yet answered.
    WithdrawVideo,
    /// The answer to [`Self::AskVideo`]: accepted or kept voice.
    AnswerVideo(bool),
    /// A kind a newer build sent that this one does not know. Ignored, never an error.
    Unknown,
}

impl From<Signal> for WireSignal {
    fn from(signal: Signal) -> Self {
        let kind = match signal {
            Signal::Offer(hello) => Some(SignalKind::Offer(hello)),
            Signal::Accept(hello) => Some(SignalKind::Accept(hello)),
            Signal::Reject => Some(SignalKind::Reject(Empty {})),
            Signal::Busy => Some(SignalKind::Busy(Empty {})),
            Signal::Hangup => Some(SignalKind::Hangup(Empty {})),
            Signal::KeyframeRequest => Some(SignalKind::KeyframeRequest(Empty {})),
            Signal::Incompatible(hello) => Some(SignalKind::Incompatible(hello)),
            Signal::Media(state) => Some(SignalKind::Media(state)),
            Signal::AskVideo => Some(SignalKind::VideoAsk(VideoAsk { withdrawn: false })),
            Signal::WithdrawVideo => Some(SignalKind::VideoAsk(VideoAsk { withdrawn: true })),
            Signal::AnswerVideo(accepted) => Some(SignalKind::VideoAnswer(VideoAnswer { accepted })),
            Signal::Unknown => None,
        };
        Self { kind }
    }
}

impl From<WireSignal> for Signal {
    fn from(wire: WireSignal) -> Self {
        match wire.kind {
            Some(SignalKind::Offer(hello)) => Self::Offer(hello),
            Some(SignalKind::Accept(hello)) => Self::Accept(hello),
            Some(SignalKind::Reject(_)) => Self::Reject,
            Some(SignalKind::Busy(_)) => Self::Busy,
            Some(SignalKind::Hangup(_)) => Self::Hangup,
            Some(SignalKind::KeyframeRequest(_)) => Self::KeyframeRequest,
            Some(SignalKind::Incompatible(hello)) => Self::Incompatible(hello),
            Some(SignalKind::Media(state)) => Self::Media(state),
            Some(SignalKind::VideoAsk(VideoAsk { withdrawn: false })) => Self::AskVideo,
            Some(SignalKind::VideoAsk(VideoAsk { withdrawn: true })) => Self::WithdrawVideo,
            Some(SignalKind::VideoAnswer(VideoAnswer { accepted })) => Self::AnswerVideo(accepted),
            None => Self::Unknown,
        }
    }
}

/// A video frame's own facts, ahead of its bytes.
#[derive(Clone, Copy, PartialEq, Message)]
pub struct FrameHeader {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(uint64, tag = "2")]
    pub capture_micros: u64,
    #[prost(bool, tag = "3")]
    pub keyframe: bool,
    /// Codec configuration (H.264 SPS/PPS) rather than a picture.
    #[prost(bool, tag = "4")]
    pub config: bool,
    /// Quarter turns the receiver applies to show the picture upright.
    #[prost(uint32, tag = "5")]
    pub turns: u32,
}

/// What a unidirectional stream carries, before anything else on it. A kind this build does not
/// know means the stream is not for it, and it is stopped.
#[derive(Clone, Copy, PartialEq, Message)]
pub struct StreamHeader {
    #[prost(oneof = "StreamKind", tags = "1")]
    pub kind: Option<StreamKind>,
}

#[derive(Clone, Copy, PartialEq, Oneof)]
pub enum StreamKind {
    #[prost(message, tag = "1")]
    Video(FrameHeader),
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct AudioHeader {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(uint64, tag = "2")]
    pub capture_micros: u64,
}

/// What a datagram carries, before its payload. A kind this build does not know is dropped.
#[derive(Clone, Copy, PartialEq, Message)]
pub struct DatagramHeader {
    #[prost(oneof = "DatagramKind", tags = "1")]
    pub kind: Option<DatagramKind>,
}

#[derive(Clone, Copy, PartialEq, Oneof)]
pub enum DatagramKind {
    #[prost(message, tag = "1")]
    Audio(AudioHeader),
}

pub(crate) fn encode<M: Message>(message: &M) -> Vec<u8> {
    message.encode_length_delimited_to_vec()
}

fn checked_length(length: u64) -> Result<usize, Error> {
    let length = usize::try_from(length).map_err(|_| Error::FrameTooLarge(usize::MAX))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(Error::FrameTooLarge(length));
    }
    Ok(length)
}

/// Splits a length-delimited message off the front of `bytes`; returns it and the rest, which for
/// a datagram is its payload.
pub(crate) fn split_message<M: Message + Default>(bytes: &[u8]) -> Result<(M, &[u8]), Error> {
    let mut rest = bytes;
    let length = checked_length(prost::encoding::decode_varint(&mut rest)?)?;
    let (body, rest) = rest.split_at_checked(length).ok_or(Error::Protocol("short message"))?;
    Ok((M::decode(body)?, rest))
}

pub async fn write_message<M: Message>(stream: &mut SendStream, message: &M) -> Result<(), Error> {
    stream.write_all(&encode(message)).await?;
    Ok(())
}

pub async fn read_message<M: Message + Default>(stream: &mut RecvStream) -> Result<M, Error> {
    let length = read_length(stream).await?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(M::decode(body.as_slice())?)
}

/// The varint in front of a message, a byte at a time: a stream has no buffer to decode it from.
async fn read_length(stream: &mut RecvStream) -> Result<usize, Error> {
    let mut length = 0u64;
    for step in 0..MAX_LENGTH_BYTES {
        let mut byte = [0u8];
        stream.read_exact(&mut byte).await?;
        length |= u64::from(byte[0] & VARINT_BITS) << (VARINT_SHIFT * step);
        if byte[0] & VARINT_MORE == 0 {
            return checked_length(length);
        }
    }
    Err(Error::Protocol("message length does not end"))
}

pub async fn send(stream: &mut SendStream, signal: Signal) -> Result<(), Error> {
    write_message(stream, &WireSignal::from(signal)).await
}

pub async fn recv(stream: &mut RecvStream) -> Result<Signal, Error> {
    Ok(read_message::<WireSignal>(stream).await?.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello::ours("0.1.test")
    }

    fn round_trip<M: Message + Default>(message: &M) -> Result<M, Error> {
        let bytes = encode(message);
        let (decoded, rest) = split_message::<M>(&bytes)?;
        assert!(rest.is_empty());
        Ok(decoded)
    }

    #[test]
    fn every_signal_round_trips() -> Result<(), Error> {
        let all = [
            Signal::Offer(hello()),
            Signal::Accept(hello()),
            Signal::Reject,
            Signal::Busy,
            Signal::Hangup,
            Signal::KeyframeRequest,
            Signal::Incompatible(hello()),
            Signal::Offer(hello().offer(Setup { call: 7, voice: true, resume: false })),
            Signal::Media(MediaState { mic_off: true, camera_off: false, held: false, ..MediaState::default() }),
            Signal::Media(MediaState { mic_off: false, camera_off: false, held: true, ..MediaState::default() }),
            Signal::AskVideo,
            Signal::WithdrawVideo,
            Signal::AnswerVideo(true),
            Signal::AnswerVideo(false),
        ];
        for signal in all {
            let decoded: Signal = round_trip(&WireSignal::from(signal.clone()))?.into();
            assert_eq!(decoded, signal);
        }
        Ok(())
    }

    /// A signal from a newer build, of a kind this one has never seen.
    #[derive(Clone, PartialEq, Message)]
    struct FutureSignal {
        #[prost(oneof = "FutureKind", tags = "99")]
        kind: Option<FutureKind>,
    }

    #[derive(Clone, PartialEq, Oneof)]
    enum FutureKind {
        #[prost(message, tag = "99")]
        CameraOff(Empty),
    }

    #[test]
    fn a_signal_from_a_newer_build_reads_as_unknown() -> Result<(), Error> {
        let future = FutureSignal { kind: Some(FutureKind::CameraOff(Empty {})) };
        let (wire, _) = split_message::<WireSignal>(&encode(&future))?;
        assert_eq!(Signal::from(wire), Signal::Unknown);
        Ok(())
    }

    /// The same header as a newer build might send it, with a field this one does not know.
    #[derive(Clone, PartialEq, Message)]
    struct FrameHeaderLater {
        #[prost(uint64, tag = "1")]
        sequence: u64,
        #[prost(uint64, tag = "2")]
        capture_micros: u64,
        #[prost(bool, tag = "3")]
        keyframe: bool,
        #[prost(bool, tag = "4")]
        config: bool,
        #[prost(uint32, tag = "5")]
        turns: u32,
        #[prost(uint32, tag = "6")]
        layer: u32,
    }

    #[test]
    fn a_field_either_side_does_not_know_is_skipped_or_defaulted() -> Result<(), Error> {
        let later =
            FrameHeaderLater { sequence: 7, capture_micros: 1_000, keyframe: true, config: false, turns: 1, layer: 2 };
        let (now, _) = split_message::<FrameHeader>(&encode(&later))?;
        assert_eq!((now.sequence, now.keyframe, now.turns), (7, true, 1));
        let (back, _) = split_message::<FrameHeaderLater>(&encode(&now))?;
        assert_eq!(back.layer, 0);
        Ok(())
    }

    /// Media state as it was before hold, which a build without it still sends and reads.
    #[derive(Clone, Copy, PartialEq, Message)]
    struct MediaStateBeforeHold {
        #[prost(bool, tag = "1")]
        mic_off: bool,
        #[prost(bool, tag = "2")]
        camera_off: bool,
    }

    #[test]
    fn hold_reads_as_not_held_where_it_is_not_known() -> Result<(), Error> {
        let before = MediaStateBeforeHold { mic_off: true, camera_off: true };
        let (now, _) = split_message::<MediaState>(&encode(&before))?;
        assert_eq!(now, MediaState { mic_off: true, camera_off: true, held: false, ..MediaState::default() });
        let held = MediaState { mic_off: false, camera_off: false, held: true, ..MediaState::default() };
        let (back, _) = split_message::<MediaStateBeforeHold>(&encode(&held))?;
        assert_eq!(back, MediaStateBeforeHold { mic_off: false, camera_off: false });
        Ok(())
    }

    /// Media state as it was before the capture ask, which older builds still send and read.
    #[derive(Clone, Copy, PartialEq, Message)]
    struct MediaStateBeforeCapture {
        #[prost(bool, tag = "1")]
        mic_off: bool,
        #[prost(bool, tag = "2")]
        camera_off: bool,
        #[prost(bool, tag = "3")]
        held: bool,
    }

    /// An older build skips the ask (and so never confirms it), and its state reads as nothing
    /// asked and nothing blocked: how "their app can't block screenshots" is known.
    #[test]
    fn a_capture_ask_is_skipped_where_it_is_not_known() -> Result<(), Error> {
        let asking = MediaState { capture_asked: true, capture_blocked: true, ..MediaState::default() };
        let (old, _) = split_message::<MediaStateBeforeCapture>(&encode(&asking))?;
        assert_eq!(old, MediaStateBeforeCapture { mic_off: false, camera_off: false, held: false });
        let (now, _) = split_message::<MediaState>(&encode(&MediaStateBeforeCapture {
            mic_off: true,
            camera_off: false,
            held: true,
        }))?;
        assert!(!now.capture_asked && !now.capture_blocked && now.mic_off && now.held);
        Ok(())
    }

    #[test]
    fn a_datagram_keeps_its_payload_after_the_header() -> Result<(), Error> {
        let header = DatagramHeader { kind: Some(DatagramKind::Audio(AudioHeader { sequence: 3, capture_micros: 9 })) };
        let payload = [1u8, 2, 3];
        let datagram = [encode(&header), payload.to_vec()].concat();
        let (read, rest) = split_message::<DatagramHeader>(&datagram)?;
        assert_eq!(read, header);
        assert_eq!(rest, payload);
        Ok(())
    }

    #[test]
    fn a_requirement_the_other_side_lacks_says_who_is_behind() {
        const FROM_THE_FUTURE: i32 = 99;
        let ours = hello();
        let mut theirs = hello();
        assert_eq!(behind(&ours, &theirs), None);
        theirs.requires.push(FROM_THE_FUTURE);
        assert_eq!(behind(&ours, &theirs), Some(Behind::Us));
        assert_eq!(behind(&theirs, &ours), Some(Behind::Them));
        // Once this side can do it too, the two can call again.
        let mut caught_up = hello();
        caught_up.supports.push(FROM_THE_FUTURE);
        assert_eq!(behind(&caught_up, &theirs), None);
    }

    #[test]
    fn oversized_messages_are_rejected() {
        let too_long = u64::try_from(MAX_MESSAGE_BYTES + 1).unwrap_or(u64::MAX);
        assert!(matches!(checked_length(too_long), Err(Error::FrameTooLarge(_))));
    }

    #[test]
    fn garbage_does_not_decode() {
        assert!(split_message::<WireSignal>(&[]).is_err());
        assert!(split_message::<WireSignal>(&[u8::MAX]).is_err());
    }
}
