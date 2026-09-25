//! Voice over QUIC datagrams: one 20 ms Opus packet (48 kHz mono, in-band FEC) per datagram,
//! never retransmitted. The receiver plays out through a jitter buffer on the caller's playback
//! clock: a lost packet is rebuilt from the next packet's FEC data, else concealed by Opus.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_int};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::Connection;
use opus_sys as ffi;
use tokio::sync::mpsc;

use crate::media::{Link, MediaStats};
use crate::protocol::{AudioHeader, DatagramHeader, DatagramKind};
use crate::{Error, protocol};

pub const SAMPLE_RATE: i32 = 48_000;
pub const CHANNELS: i32 = 1;
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
/// [`FRAME_DURATION`] at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 960;
pub type Pcm = [i16; FRAME_SAMPLES];

/// Until [`AudioSender::set_bitrate`] says otherwise: Balanced's, what calls always sent.
const BITRATE: i32 = 32_000;
/// Loss the encoder plans FEC for until rate control measures the real figure.
const EXPECTED_LOSS_PERCENT: i32 = 10;
const ENABLED: i32 = 1;
/// Largest Opus packet for one frame (RFC 6716).
const MAX_PACKET_BYTES: usize = 1275;
const INCOMING_PACKET_QUEUE: usize = 64;
/// Playout starts once this much audio is buffered (60 ms).
const PREBUFFER_FRAMES: usize = 3;
/// Beyond this (200 ms) the oldest audio is skipped to keep latency bounded.
const MAX_BUFFERED_FRAMES: usize = 10;
/// After this much consecutive concealment (200 ms) playout stops and re-buffers.
const MAX_CONCEALED_RUN: u32 = 10;

fn check(code: c_int) -> Result<c_int, Error> {
    if code >= 0 {
        return Ok(code);
    }
    // SAFETY: opus_strerror returns a static string for any code.
    let message = unsafe { CStr::from_ptr(ffi::opus_strerror(code)) };
    Err(Error::Opus(message.to_string_lossy().into_owned()))
}

#[derive(Debug)]
struct Encoder(NonNull<ffi::OpusEncoder>);

// SAFETY: the encoder state is plain heap memory with no thread affinity; it is used through
// `&mut self` only.
unsafe impl Send for Encoder {}

impl Encoder {
    fn new() -> Result<Self, Error> {
        let mut status = ffi::OPUS_OK;
        // SAFETY: valid parameters; `status` receives the error code.
        let state =
            unsafe { ffi::opus_encoder_create(SAMPLE_RATE, CHANNELS, ffi::OPUS_APPLICATION_VOIP, &raw mut status) };
        check(status)?;
        let encoder = Self(NonNull::new(state).ok_or_else(|| Error::Opus("encoder allocation failed".into()))?);
        encoder.ctl(ffi::OPUS_SET_BITRATE_REQUEST, BITRATE)?;
        encoder.ctl(ffi::OPUS_SET_INBAND_FEC_REQUEST, ENABLED)?;
        encoder.ctl(ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, EXPECTED_LOSS_PERCENT)?;
        Ok(encoder)
    }

    fn ctl(&self, request: c_int, value: c_int) -> Result<(), Error> {
        // SAFETY: every request used here takes one `opus_int32` argument.
        check(unsafe { ffi::opus_encoder_ctl(self.0.as_ptr(), request, value) }).map(drop)
    }

    fn encode(&mut self, pcm: &Pcm, packet: &mut [u8]) -> Result<usize, Error> {
        let frame = c_int::try_from(pcm.len()).map_err(|_| Error::Opus("frame too long".into()))?;
        let capacity = i32::try_from(packet.len()).unwrap_or(i32::MAX);
        // SAFETY: `pcm` holds `frame` samples and `packet` `capacity` bytes.
        let written =
            check(unsafe { ffi::opus_encode(self.0.as_ptr(), pcm.as_ptr(), frame, packet.as_mut_ptr(), capacity) })?;
        Ok(usize::try_from(written).unwrap_or_default())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and destroyed once.
        unsafe { ffi::opus_encoder_destroy(self.0.as_ptr()) };
    }
}

#[derive(Debug)]
struct Decoder(NonNull<ffi::OpusDecoder>);

// SAFETY: as for `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    fn new() -> Result<Self, Error> {
        let mut status = ffi::OPUS_OK;
        // SAFETY: valid parameters; `status` receives the error code.
        let state = unsafe { ffi::opus_decoder_create(SAMPLE_RATE, CHANNELS, &raw mut status) };
        check(status)?;
        Ok(Self(NonNull::new(state).ok_or_else(|| Error::Opus("decoder allocation failed".into()))?))
    }

    /// Decodes `packet` (its FEC data when `fec`), or conceals a loss when `None`.
    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, pcm: &mut Pcm) -> Result<(), Error> {
        let (data, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), i32::try_from(p.len()).unwrap_or(0)));
        let frame = c_int::try_from(pcm.len()).map_err(|_| Error::Opus("frame too long".into()))?;
        // SAFETY: `data` holds `len` bytes (or is null for concealment); `pcm` holds `frame` samples.
        let decoded =
            check(unsafe { ffi::opus_decode(self.0.as_ptr(), data, len, pcm.as_mut_ptr(), frame, c_int::from(fec)) })?;
        // A short decode (never expected for 20 ms packets) leaves stale samples otherwise.
        if let Some(rest) = pcm.get_mut(usize::try_from(decoded).unwrap_or_default()..) {
            rest.fill(0);
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and destroyed once.
        unsafe { ffi::opus_decoder_destroy(self.0.as_ptr()) };
    }
}

/// Encodes and sends our voice; call once per captured 20 ms frame.
#[derive(Debug)]
pub struct AudioSender {
    link: Link,
    encoder: Encoder,
    sequence: u64,
    packet: Vec<u8>,
    stats: Arc<MediaStats>,
}

impl AudioSender {
    /// The most the voice may take, in bits a second, from the next frame on.
    pub fn set_bitrate(&self, bps: i32) -> Result<(), Error> {
        self.encoder.ctl(ffi::OPUS_SET_BITRATE_REQUEST, bps)
    }

    /// A full datagram buffer drops the frame: late audio is worse than lost audio. So does a lost
    /// connection: whether the call is over is the call's to decide, and it may yet rejoin.
    pub fn send(&mut self, pcm: &Pcm, capture_micros: u64) -> Result<(), Error> {
        let len = self.encoder.encode(pcm, &mut self.packet)?;
        let header =
            DatagramHeader { kind: Some(DatagramKind::Audio(AudioHeader { sequence: self.sequence, capture_micros })) };
        self.sequence += 1;
        let mut datagram = protocol::encode(&header);
        datagram.extend_from_slice(self.packet.get(..len).unwrap_or_default());
        let bytes = u64::try_from(datagram.len()).unwrap_or(u64::MAX);
        let connection = self.link.borrow().clone();
        match connection.send_datagram(datagram.into()) {
            Ok(()) => {
                MediaStats::count(&self.stats.audio_sent, 1);
                MediaStats::count(&self.stats.bytes_sent, bytes);
            }
            Err(e) => {
                tracing::debug!("audio packet dropped: {e}");
                MediaStats::count(&self.stats.audio_send_dropped, 1);
            }
        }
        Ok(())
    }
}

/// Plays out the peer's voice; call once per 20 ms of playback.
#[derive(Debug)]
pub struct AudioReceiver {
    incoming: mpsc::Receiver<(u64, Vec<u8>)>,
    buffer: JitterBuffer,
    decoder: Decoder,
    stats: Arc<MediaStats>,
}

impl AudioReceiver {
    /// Fills `pcm` with the next 20 ms: decoded, FEC-recovered, concealed, or silence while
    /// (re)buffering. Returns the packet that was played as received, with its sequence number
    /// (for recording); `None` when this frame was rebuilt or silent.
    pub fn next(&mut self, pcm: &mut Pcm) -> Result<Option<(u64, Vec<u8>)>, Error> {
        while let Ok((sequence, packet)) = self.incoming.try_recv() {
            if !self.buffer.push(sequence, packet) {
                MediaStats::count(&self.stats.audio_late, 1);
            }
        }
        let playout = self.buffer.pop();
        MediaStats::count(&self.stats.audio_skipped, std::mem::take(&mut self.buffer.skipped));
        match playout {
            Playout::Packet(sequence, packet) => {
                self.decoder.decode(Some(&packet), false, pcm)?;
                return Ok(Some((sequence, packet)));
            }
            Playout::Fec(following) => {
                MediaStats::count(&self.stats.audio_fec_recovered, 1);
                self.decoder.decode(Some(&following), true, pcm)?;
            }
            Playout::Conceal => {
                MediaStats::count(&self.stats.audio_concealed, 1);
                self.decoder.decode(None, false, pcm)?;
            }
            Playout::Silence => pcm.fill(0),
        }
        Ok(None)
    }
}

pub(crate) fn start(link: &Link, stats: &Arc<MediaStats>) -> Result<(AudioSender, AudioReceiver), Error> {
    let sender = AudioSender {
        link: link.clone(),
        encoder: Encoder::new()?,
        sequence: 0,
        packet: vec![0; MAX_PACKET_BYTES],
        stats: Arc::clone(stats),
    };
    let (tx, incoming) = mpsc::channel(INCOMING_PACKET_QUEUE);
    let receiver =
        AudioReceiver { incoming, buffer: JitterBuffer::default(), decoder: Decoder::new()?, stats: Arc::clone(stats) };
    tokio::spawn(receive(link.clone(), tx, Arc::clone(stats)));
    Ok((sender, receiver))
}

/// Hands each datagram to the receiver, over each connection the call has, until it ends.
async fn receive(mut link: Link, deliver: mpsc::Sender<(u64, Vec<u8>)>, stats: Arc<MediaStats>) {
    loop {
        let connection = link.borrow_and_update().clone();
        let lost = tokio::select! {
            () = receive_on(&connection, &deliver, &stats) => true,
            changed = link.changed() => {
                if changed.is_err() {
                    break;
                }
                false
            }
        };
        // Wait for the call to rejoin on a new connection, or to end.
        if lost && link.changed().await.is_err() {
            break;
        }
    }
}

async fn receive_on(connection: &Connection, deliver: &mpsc::Sender<(u64, Vec<u8>)>, stats: &MediaStats) {
    while let Ok(datagram) = connection.read_datagram().await {
        let (header, packet) = match protocol::split_message::<DatagramHeader>(&datagram) {
            Ok((DatagramHeader { kind: Some(DatagramKind::Audio(header)) }, packet)) => (header, packet),
            // From a newer build, carrying something this one does not handle.
            Ok((DatagramHeader { kind: None }, _)) => {
                tracing::debug!("a datagram of a kind this build does not know");
                continue;
            }
            Err(e) => {
                tracing::debug!("bad audio datagram: {e}");
                continue;
            }
        };
        MediaStats::count(&stats.audio_received, 1);
        MediaStats::count(&stats.bytes_received, u64::try_from(datagram.len()).unwrap_or(u64::MAX));
        // The receiver drains every 20 ms; a full queue means playback stalled.
        if deliver.try_send((header.sequence, packet.to_vec())).is_err() {
            MediaStats::count(&stats.audio_late, 1);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Playout {
    Packet(u64, Vec<u8>),
    /// The expected packet is missing; this is the one after it, whose FEC rebuilds it.
    Fec(Vec<u8>),
    Conceal,
    Silence,
}

/// Orders packets and decides what to play each 20 ms; pure logic.
#[derive(Debug, Default)]
struct JitterBuffer {
    packets: BTreeMap<u64, Vec<u8>>,
    /// Next sequence to play; `None` while (re)buffering.
    next: Option<u64>,
    concealed_run: u32,
    /// Packets skipped to bound latency, taken by the receiver for stats.
    skipped: u64,
}

impl JitterBuffer {
    /// Returns false for a packet that arrived after its playout time.
    fn push(&mut self, sequence: u64, packet: Vec<u8>) -> bool {
        if self.next.is_some_and(|next| sequence < next) {
            return false;
        }
        self.packets.insert(sequence, packet);
        true
    }

    fn pop(&mut self) -> Playout {
        if self.next.is_none() {
            if self.packets.len() < PREBUFFER_FRAMES {
                return Playout::Silence;
            }
            self.next = self.packets.keys().next().copied();
        }
        if self.packets.len() > MAX_BUFFERED_FRAMES {
            while self.packets.len() > PREBUFFER_FRAMES {
                self.packets.pop_first();
                self.skipped += 1;
            }
            self.next = self.packets.keys().next().copied();
        }
        let Some(next) = self.next else { return Playout::Silence };
        self.next = Some(next + 1);
        if let Some(packet) = self.packets.remove(&next) {
            self.concealed_run = 0;
            return Playout::Packet(next, packet);
        }
        if let Some(following) = self.packets.get(&(next + 1)) {
            self.concealed_run = 0;
            return Playout::Fec(following.clone());
        }
        self.concealed_run += 1;
        if self.concealed_run > MAX_CONCEALED_RUN {
            self.next = None;
            self.concealed_run = 0;
            return Playout::Silence;
        }
        Playout::Conceal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u64) -> Vec<u8> {
        sequence.to_le_bytes().to_vec()
    }

    fn played(sequence: u64) -> Playout {
        Playout::Packet(sequence, packet(sequence))
    }

    fn filled(sequences: impl IntoIterator<Item = u64>) -> JitterBuffer {
        let mut buffer = JitterBuffer::default();
        for sequence in sequences {
            buffer.push(sequence, packet(sequence));
        }
        buffer
    }

    #[test]
    fn prebuffers_then_plays_in_order() {
        let mut buffer = filled([1, 0]);
        assert_eq!(buffer.pop(), Playout::Silence);
        buffer.push(2, packet(2));
        for sequence in 0..3 {
            assert_eq!(buffer.pop(), played(sequence));
        }
    }

    #[test]
    fn a_gap_uses_the_next_packets_fec_then_conceals() {
        let mut buffer = filled([0, 2, 3]);
        assert_eq!(buffer.pop(), played(0));
        assert_eq!(buffer.pop(), Playout::Fec(packet(2)));
        assert_eq!(buffer.pop(), played(2));
        assert_eq!(buffer.pop(), played(3));
        assert_eq!(buffer.pop(), Playout::Conceal);
    }

    #[test]
    fn late_packets_are_refused() {
        let mut buffer = filled(0..3);
        buffer.pop();
        buffer.pop();
        assert!(!buffer.push(0, packet(0)));
        assert!(buffer.push(5, packet(5)));
    }

    #[test]
    fn long_loss_rebuffers() {
        let mut buffer = filled(0..3);
        for _ in 0..3 {
            buffer.pop();
        }
        for _ in 0..MAX_CONCEALED_RUN {
            assert_eq!(buffer.pop(), Playout::Conceal);
        }
        assert_eq!(buffer.pop(), Playout::Silence);
        assert_eq!(buffer.next, None);
    }

    #[test]
    fn backlog_is_skipped_down_to_the_prebuffer() {
        let frames = u64::try_from(MAX_BUFFERED_FRAMES).unwrap_or(u64::MAX) + 1;
        let mut buffer = filled(0..frames);
        let kept = u64::try_from(PREBUFFER_FRAMES).unwrap_or(u64::MAX);
        assert_eq!(buffer.pop(), played(frames - kept));
        assert_eq!(buffer.skipped, frames - kept);
    }

    #[test]
    fn frame_constants_agree() {
        let samples = u128::try_from(SAMPLE_RATE).unwrap_or_default() * FRAME_DURATION.as_millis() / 1000;
        assert_eq!(usize::try_from(samples).ok(), Some(FRAME_SAMPLES));
    }

    #[test]
    fn opus_round_trip_and_concealment() -> Result<(), Error> {
        let (mut encoder, mut decoder) = (Encoder::new()?, Decoder::new()?);
        let tone: Pcm = std::array::from_fn(|i| if i % 48 < 24 { 8000 } else { -8000 });
        let mut packet = vec![0; MAX_PACKET_BYTES];
        let len = encoder.encode(&tone, &mut packet)?;
        assert!(len > 0 && len < MAX_PACKET_BYTES);
        let mut pcm = [0; FRAME_SAMPLES];
        decoder.decode(packet.get(..len), false, &mut pcm)?;
        decoder.decode(None, false, &mut pcm)?;
        decoder.decode(packet.get(..len), true, &mut pcm)?;
        Ok(())
    }
}
