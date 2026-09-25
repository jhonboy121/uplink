//! Records a call's incoming media to one fragmented MP4: H.264 frames as received (parameter
//! sets moved into `avcC`) and Opus packets as played out, without transcoding. Written one
//! fragment at a time, so an abrupt end still leaves a playable file.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use mp4_atom::{
    Audio, Avc1, Avcc, Codec, Dinf, Dops, Dref, Encode, FixedPoint, Ftyp, Hdlr, Matrix, Mdat, Mdhd, Mdia, Mfhd,
    Minf, Moof, Moov, Mvex, Mvhd, Opus, Smhd, Stbl, Stco, Stsd, Tfdt, Tfhd, Tkhd, Traf, Trak, Trex, Trun, TrunEntry, Url,
    Visual, Vmhd,
};
use uplink_core::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use uplink_core::media::Frame;

use h264::nal::{AUD, PPS, SPS, nal_type};
use h264::{nal_units, sps_dimensions};

const VIDEO_TRACK: u32 = 1;
const AUDIO_TRACK: u32 = 2;
/// Video times are the sender's capture microseconds.
const VIDEO_TIMESCALE: u32 = 1_000_000;
const MICROS_PER_SECOND: u64 = 1_000_000;
/// Duration for a fragment's last video frame, whose successor hasn't arrived yet.
const FALLBACK_FRAME_MICROS: u32 = 33_333;
/// `trun` sample flags (ISO/IEC 14496-12 8.8.3.1): depends on nothing / depends on others and
/// is not a sync sample.
const SYNC_SAMPLE: u32 = 0x0200_0000;
const NON_SYNC_SAMPLE: u32 = 0x0101_0000;
/// Opus encoder lookahead at 48 kHz for the VoIP application (2.5 ms + 4 ms delay compensation).
const OPUS_PRE_SKIP: u16 = 312;
const MONO: u16 = 1;
const SAMPLE_BITS: u16 = 16;
/// 16.16 fixed point 1.0, for the display matrix.
const FIXED_ONE: i32 = 0x0001_0000;
const MDAT_HEADER: usize = 8;
const TURNS_PER_REVOLUTION: u8 = 4;
/// How much media each fragment holds, i.e. what an abrupt end can lose.
pub const FRAGMENT_INTERVAL: Duration = Duration::from_secs(1);

struct Pending {
    time: u64,
    data: Vec<u8>,
    sync: bool,
}

/// The `moov` as written, so its durations can be filled in when the recording closes.
struct Init {
    offset: u64,
    len: usize,
    moov: Moov,
}

pub struct Recorder {
    out: BufWriter<File>,
    started: Instant,
    fragment: u32,
    /// The init segment is written at the first keyframe (it needs the SPS/PPS).
    init: Option<Init>,
    /// End of the media written so far, in each track's own timescale.
    video_end: u64,
    audio_end: u64,
    /// (first capture micros, its media time): media times follow local arrival of the first frame.
    video_origin: Option<(u64, u64)>,
    /// (first sequence, its media time in samples).
    audio_origin: Option<(u64, u64)>,
    video: Vec<Pending>,
    audio: Vec<Pending>,
}

impl Recorder {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        tracing::info!("recording to {}", path.display());
        Ok(Self {
            out: BufWriter::new(file),
            started: Instant::now(),
            fragment: 0,
            init: None,
            video_end: 0,
            audio_end: 0,
            video_origin: None,
            audio_origin: None,
            video: Vec::new(),
            audio: Vec::new(),
        })
    }

    fn elapsed_micros(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    pub fn video(&mut self, frame: &Frame) -> Result<()> {
        let nals: Vec<&[u8]> = nal_units(&frame.data).collect();
        if self.init.is_none() {
            let find = |kind| nals.iter().find(|nal| nal_type(nal) == Some(kind)).copied();
            let (Some(sps), Some(pps)) = (find(SPS), find(PPS)) else { return Ok(()) };
            self.write_init(sps, pps, frame.turns)?;
        }
        let mut data = Vec::with_capacity(frame.data.len());
        for nal in nals.iter().filter(|nal| !matches!(nal_type(nal), Some(SPS | PPS | AUD))) {
            data.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
            data.extend_from_slice(nal);
        }
        let now = self.elapsed_micros();
        let (first_capture, first_time) = *self.video_origin.get_or_insert((frame.capture_micros, now));
        let time = first_time + frame.capture_micros.saturating_sub(first_capture);
        self.video_end = self.video_end.max(time + u64::from(FALLBACK_FRAME_MICROS));
        self.video.push(Pending { time, data, sync: frame.keyframe });
        Ok(())
    }

    pub fn audio(&mut self, sequence: u64, packet: Vec<u8>) {
        if self.init.is_none() {
            return;
        }
        let now = self.elapsed_micros() * u64::try_from(SAMPLE_RATE).unwrap_or_default() / MICROS_PER_SECOND;
        let (first_sequence, first_time) = *self.audio_origin.get_or_insert((sequence, now));
        let frame = u64::try_from(FRAME_SAMPLES).unwrap_or_default();
        let time = first_time + sequence.saturating_sub(first_sequence) * frame;
        self.audio_end = self.audio_end.max(time + frame);
        self.audio.push(Pending { time, data: packet, sync: true });
    }

    fn write_init(&mut self, sps: &[u8], pps: &[u8], turns: u8) -> Result<()> {
        let (width, height) = sps_dimensions(sps).context("unreadable SPS")?;
        let (width, height) = (u16::try_from(width)?, u16::try_from(height)?);
        tracing::info!("recording {width}x{height}, rotated {turns} quarter turns");
        let dinf = Dinf { dref: Dref { urls: vec![Url { location: String::new() }] } };
        let video = Trak {
            tkhd: Tkhd {
                track_id: VIDEO_TRACK,
                enabled: true,
                in_movie: true,
                matrix: rotation(turns),
                width: FixedPoint::new(width, 0),
                height: FixedPoint::new(height, 0),
                ..Tkhd::default()
            },
            mdia: Mdia {
                mdhd: Mdhd { timescale: VIDEO_TIMESCALE, language: "und".into(), ..Mdhd::default() },
                hdlr: Hdlr { handler: b"vide".into(), name: "uplink video".into() },
                minf: Minf {
                    vmhd: Some(Vmhd::default()),
                    dinf: dinf.clone(),
                    stbl: sample_entry(Codec::Avc1(Avc1 {
                        visual: Visual { width, height, ..Visual::default() },
                        avcc: Avcc::new(sps, pps)?,
                        ..Avc1::default()
                    })),
                    ..Minf::default()
                },
            },
            ..Trak::default()
        };
        let sample_rate = u32::try_from(SAMPLE_RATE)?;
        let audio = Trak {
            tkhd: Tkhd {
                track_id: AUDIO_TRACK,
                enabled: true,
                in_movie: true,
                volume: FixedPoint::new(1, 0),
                ..Tkhd::default()
            },
            mdia: Mdia {
                mdhd: Mdhd { timescale: sample_rate, language: "und".into(), ..Mdhd::default() },
                hdlr: Hdlr { handler: b"soun".into(), name: "uplink audio".into() },
                minf: Minf {
                    smhd: Some(Smhd::default()),
                    dinf,
                    stbl: sample_entry(Codec::Opus(Opus {
                        audio: Audio {
                            data_reference_index: 1,
                            channel_count: MONO,
                            sample_size: SAMPLE_BITS,
                            sample_rate: FixedPoint::new(u16::try_from(sample_rate).unwrap_or(u16::MAX), 0),
                        },
                        dops: Dops {
                            output_channel_count: u8::try_from(MONO)?,
                            pre_skip: OPUS_PRE_SKIP,
                            input_sample_rate: sample_rate,
                            output_gain: 0,
                        },
                        btrt: None,
                    })),
                    ..Minf::default()
                },
            },
            ..Trak::default()
        };
        let trex = |track_id| Trex { track_id, default_sample_description_index: 1, ..Trex::default() };
        let moov = Moov {
            mvhd: Mvhd {
                rate: FixedPoint::new(1, 0),
                volume: FixedPoint::new(1, 0),
                next_track_id: AUDIO_TRACK + 1,
                ..Mvhd::default()
            },
            mvex: Some(Mvex { mehd: None, trex: vec![trex(VIDEO_TRACK), trex(AUDIO_TRACK)] }),
            trak: vec![video, audio],
            ..Moov::default()
        };
        // `iso5`/`dash`/`msdh` tell players (Android's extractor especially) to expect fragments.
        let ftyp = Ftyp {
            major_brand: b"iso5".into(),
            minor_version: 0,
            compatible_brands: vec![
                b"iso5".into(),
                b"iso6".into(),
                b"isom".into(),
                b"dash".into(),
                b"msdh".into(),
                b"avc1".into(),
            ],
        };
        let mut header = Vec::new();
        ftyp.encode(&mut header)?;
        self.out.write_all(&header)?;
        self.out.flush()?;
        let offset = self.out.get_mut().stream_position()?;
        let mut encoded = Vec::new();
        moov.encode(&mut encoded)?;
        self.out.write_all(&encoded)?;
        self.init = Some(Init { offset, len: encoded.len(), moov });
        Ok(())
    }

    /// Fills in the durations the init segment was written without, so players that don't scan
    /// the fragments still see the real length. Sizes don't change: these fields are fixed width.
    fn write_durations(&mut self) -> Result<()> {
        let Some(Init { offset, len, moov }) = &mut self.init else { return Ok(()) };
        let movie_timescale = moov.mvhd.timescale;
        let rescale = |ticks: u64, timescale: u32| {
            ticks.saturating_mul(u64::from(movie_timescale)).checked_div(u64::from(timescale)).unwrap_or_default()
        };
        let sample_rate = u32::try_from(SAMPLE_RATE)?;
        moov.mvhd.duration = rescale(self.video_end, VIDEO_TIMESCALE).max(rescale(self.audio_end, sample_rate));
        for trak in &mut moov.trak {
            let (end, timescale) = if trak.tkhd.track_id == VIDEO_TRACK {
                (self.video_end, VIDEO_TIMESCALE)
            } else {
                (self.audio_end, sample_rate)
            };
            trak.tkhd.duration = rescale(end, timescale);
            trak.mdia.mdhd.duration = end;
        }
        let mut encoded = Vec::new();
        moov.encode(&mut encoded)?;
        self.out.flush()?;
        let end = self.out.get_mut().stream_position()?;
        // Rewriting in place only works while the box keeps its size (fixed-width v1 fields).
        ensure!(encoded.len() == *len, "moov changed size; durations not written");
        self.out.get_mut().seek(SeekFrom::Start(*offset))?;
        self.out.get_mut().write_all(&encoded)?;
        self.out.get_mut().seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Writes buffered samples as one fragment (`moof` + `mdat`).
    pub fn flush(&mut self) -> Result<()> {
        if self.init.is_none() || (self.video.is_empty() && self.audio.is_empty()) {
            return Ok(());
        }
        let frame = u32::try_from(FRAME_SAMPLES)?;
        let tracks = [
            (VIDEO_TRACK, std::mem::take(&mut self.video), FALLBACK_FRAME_MICROS),
            (AUDIO_TRACK, std::mem::take(&mut self.audio), frame),
        ];
        let mut trafs = Vec::new();
        let mut payload = Vec::new();
        for (track_id, samples, fallback) in &tracks {
            let Some(first) = samples.first() else { continue };
            let entries = samples
                .iter()
                .enumerate()
                .map(|(i, sample)| {
                    let next = samples.get(i + 1).map(|next| next.time.saturating_sub(sample.time));
                    TrunEntry {
                        duration: Some(next.and_then(|d| u32::try_from(d).ok()).unwrap_or(*fallback)),
                        size: Some(u32::try_from(sample.data.len()).unwrap_or(u32::MAX)),
                        flags: (*track_id == VIDEO_TRACK)
                            .then_some(if sample.sync { SYNC_SAMPLE } else { NON_SYNC_SAMPLE }),
                        cts: None,
                    }
                })
                .collect();
            trafs.push((
                payload.len(),
                Traf {
                    tfhd: Tfhd { track_id: *track_id, default_base_is_moof: true, ..Tfhd::default() },
                    tfdt: Some(Tfdt { base_media_decode_time: first.time }),
                    trun: vec![Trun { data_offset: Some(0), entries }],
                    ..Traf::default()
                },
            ));
            for sample in samples {
                payload.extend_from_slice(&sample.data);
            }
        }
        self.fragment += 1;
        let mut moof = Moof {
            mfhd: Mfhd { sequence_number: self.fragment },
            traf: trafs.iter().map(|(_, traf)| traf.clone()).collect(),
        };
        // Data offsets count from the start of the moof, whose size doesn't depend on them.
        let mut probe = Vec::new();
        moof.encode(&mut probe)?;
        for (traf, (start, _)) in moof.traf.iter_mut().zip(&trafs) {
            let offset = i32::try_from(probe.len() + MDAT_HEADER + start)?;
            for trun in &mut traf.trun {
                trun.data_offset = Some(offset);
            }
        }
        let mut fragment = Vec::with_capacity(probe.len() + MDAT_HEADER + payload.len());
        moof.encode(&mut fragment)?;
        Mdat { data: payload }.encode(&mut fragment)?;
        self.out.write_all(&fragment)?;
        Ok(self.out.flush()?)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            tracing::warn!("recording: last fragment lost: {e:#}");
        }
        if let Err(e) = self.write_durations() {
            tracing::warn!("recording: durations not written: {e:#}");
        }
    }
}

/// An empty sample table: fragments carry the samples, but ISO still wants the boxes present
/// (a missing `stco` is enough for strict parsers to reject the track).
fn sample_entry(codec: Codec) -> Stbl {
    Stbl { stsd: Stsd { codecs: vec![codec] }, stco: Some(Stco::default()), ..Stbl::default() }
}

/// Display matrix for `turns` quarter turns (the inverse of how clips are read).
fn rotation(turns: u8) -> Matrix {
    let (a, b) = match turns % TURNS_PER_REVOLUTION {
        1 => (0, FIXED_ONE),
        2 => (-FIXED_ONE, 0),
        3 => (0, -FIXED_ONE),
        _ => (FIXED_ONE, 0),
    };
    Matrix { a, b, c: -b, d: a, ..Matrix::default() }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use mp4_atom::{Any, Atom, ReadFrom};
    use uplink_core::media::Frame;

    use super::*;
    use crate::clip::Clip;

    /// The user's test clip; the recorder needs real H.264 to write a decodable file.
    fn source() -> Option<PathBuf> {
        let clip = PathBuf::from(std::env::var_os("HOME")?).join("uplink-media/clip.mp4");
        clip.exists().then_some(clip)
    }

    /// Writes the clip's first frames back out as a recording, for inspection with ffprobe.
    #[test]
    fn recording_is_a_readable_fragmented_mp4() -> Result<()> {
        let Some(path) = source() else { return Ok(()) };
        let clip = Clip::open(&path)?;
        let out = std::env::temp_dir().join("uplink-recorder-test.mp4");
        {
            let mut recorder = Recorder::create(&out)?;
            for sample in clip.video.samples.iter().take(60) {
                let mut data = vec![0; usize::try_from(sample.size)?];
                std::os::unix::fs::FileExt::read_exact_at(&clip.file, &mut data, sample.offset)?;
                let config = sample.sync.then_some(clip.video.config.as_slice());
                let micros = sample.decode_time * MICROS_PER_SECOND / u64::from(clip.video.timescale);
                recorder.video(&Frame {
                    capture_micros: micros,
                    keyframe: sample.sync,
                    config: false,
                    turns: clip.video.turns,
                    data: crate::clip::annex_b(&data, clip.video.length_size, config)?,
                })?;
            }
        }

        let mut file = File::open(&out)?;
        let mut kinds = Vec::new();
        while let Ok(atom) = Any::read_from(&mut file) {
            kinds.push(atom.kind());
        }
        assert!(kinds.contains(&Ftyp::KIND), "no ftyp in {kinds:?}");
        assert!(kinds.contains(&Moov::KIND), "no moov in {kinds:?}");
        assert!(kinds.contains(&Moof::KIND) && kinds.contains(&Mdat::KIND), "no fragment in {kinds:?}");
        Ok(())
    }
}
