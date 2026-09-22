//! Call media for the CLI: plays an H.264 mp4 as our video (no re-encoding) and prints what the
//! peer sends back.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mp4::{MediaType, Mp4Reader, TrackType};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uplink_core::media::{Frame, MediaSession, MediaStats, VideoSender};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);
const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// avcC `lengthSizeMinusOne` is a 2-bit field.
const LENGTH_SIZE_MASK: u8 = 0b11;
const MICROS_PER_SECOND: u64 = 1_000_000;
const BITS_PER_BYTE: f64 = 8.0;
const BITS_PER_KBIT: f64 = 1000.0;
const BYTES_PER_KIB: u32 = 1024;
const QUARTER_TURN_DEGREES: u16 = 90;

/// The video track of an mp4, read sample by sample.
pub struct Clip {
    reader: Mp4Reader<BufReader<File>>,
    track: u32,
    samples: u32,
    timescale: u64,
    /// Bytes of each NAL length prefix in the samples.
    length_size: usize,
    /// SPS + PPS in Annex-B form, prepended to every keyframe.
    config: Vec<u8>,
    turns: u8,
}

impl Clip {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata()?.len();
        let reader = Mp4Reader::read_header(BufReader::new(file), size)?;
        let track = reader
            .tracks()
            .values()
            .find(|t| matches!(t.track_type(), Ok(TrackType::Video)))
            .context("no video track")?;
        if !matches!(track.media_type(), Ok(MediaType::H264)) {
            bail!("video is {:?}, need H.264", track.media_type()?);
        }
        let Some(avc1) = &track.trak.mdia.minf.stbl.stsd.avc1 else { bail!("no avc1 sample entry") };
        let avcc = &avc1.avcc;
        let config = avcc
            .sequence_parameter_sets
            .iter()
            .chain(&avcc.picture_parameter_sets)
            .flat_map(|nal| [START_CODE.as_slice(), &nal.bytes].concat())
            .collect();
        // Display rotation from the `tkhd` matrix (16.16 fixed point; the signs of a and b suffice).
        let matrix = &track.trak.tkhd.matrix;
        let turns = match (matrix.a.signum(), matrix.b.signum()) {
            (0, 1) => 1,
            (-1, 0) => 2,
            (0, -1) => 3,
            _ => 0,
        };
        println!(
            "clip: {}x{} {:.2} fps {:.0} kbps, {} samples, {:?}, rotated {}°",
            track.width(),
            track.height(),
            track.frame_rate(),
            f64::from(track.bitrate()) / BITS_PER_KBIT,
            track.sample_count(),
            track.duration(),
            u16::from(turns) * QUARTER_TURN_DEGREES,
        );
        Ok(Self {
            track: track.track_id(),
            samples: track.sample_count(),
            timescale: u64::from(track.timescale()),
            length_size: usize::from(avcc.length_size_minus_one & LENGTH_SIZE_MASK) + 1,
            config,
            turns,
            reader,
        })
    }

    fn duration(&self, ticks: u64) -> Duration {
        Duration::from_micros(ticks.saturating_mul(MICROS_PER_SECOND).checked_div(self.timescale).unwrap_or_default())
    }

    /// Sends the clip in real time, looping; returns only on a read error.
    async fn play(mut self, video: &mut VideoSender) -> Result<()> {
        let start = Instant::now();
        let mut loop_offset = Duration::ZERO;
        loop {
            let mut end = 0;
            for id in 1..=self.samples {
                let Some(sample) = self.reader.read_sample(self.track, id)? else { continue };
                end = sample.start_time.saturating_add(u64::from(sample.duration));
                tokio::time::sleep_until(start + loop_offset + self.duration(sample.start_time)).await;
                let presentation = sample.start_time.saturating_add_signed(i64::from(sample.rendering_offset));
                let capture = loop_offset + self.duration(presentation);
                let config = sample.is_sync.then_some(self.config.as_slice());
                video.send(Frame {
                    capture_micros: u64::try_from(capture.as_micros()).unwrap_or(u64::MAX),
                    keyframe: sample.is_sync,
                    config: false,
                    turns: self.turns,
                    data: annex_b(&sample.bytes, self.length_size, config)?,
                });
            }
            loop_offset += self.duration(end);
        }
    }
}

/// Length-prefixed NAL units (mp4 samples) → Annex-B, optionally led by `config`.
fn annex_b(sample: &[u8], length_size: usize, config: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut out = config.map(<[u8]>::to_vec).unwrap_or_default();
    let mut rest = sample;
    while !rest.is_empty() {
        let (prefix, tail) = rest.split_at_checked(length_size).context("truncated NAL length")?;
        let length = prefix.iter().fold(0, |acc, &b| acc << u8::BITS | usize::from(b));
        let (nal, tail) = tail.split_at_checked(length).context("truncated NAL")?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        rest = tail;
    }
    Ok(out)
}

/// Runs a call's media until the call ends (the peer's video channel closes).
pub async fn run(media: MediaSession, clip: Option<Clip>) {
    let MediaSession { mut video, incoming_video, keyframe_requests, stats } = media;
    // Never completes: holding `video` keeps our side of the media alive for the whole call.
    let play = async move {
        if let Some(clip) = clip
            && let Err(e) = clip.play(&mut video).await
        {
            println!("clip playback stopped: {e:#}");
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        () = play => {}
        () = report(incoming_video, keyframe_requests, &stats) => {}
    }
}

#[derive(Default)]
struct Window {
    frames: u32,
    bytes: u32,
    keyframes: u32,
    largest: u32,
    turns: u8,
}

impl Window {
    fn add(&mut self, frame: &Frame) {
        let bytes = u32::try_from(frame.data.len()).unwrap_or(u32::MAX);
        self.frames += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.keyframes += u32::from(frame.keyframe);
        self.largest = self.largest.max(bytes);
        self.turns = frame.turns;
    }

    fn line(&self, secs: f64, stats: &MediaStats) -> String {
        let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        format!(
            "rx {:.1} fps {:.0} kbps · {} key · max {} KiB · turns {} | sent {} (late {}, congested {}) · \
             received {} (dropped {}) · keyframe asks sent {} received {}",
            f64::from(self.frames) / secs,
            f64::from(self.bytes) * BITS_PER_BYTE / BITS_PER_KBIT / secs,
            self.keyframes,
            self.largest / BYTES_PER_KIB,
            self.turns,
            count(&stats.frames_sent),
            count(&stats.frames_late),
            count(&stats.frames_dropped_congested),
            count(&stats.frames_received),
            count(&stats.frames_dropped_received),
            count(&stats.keyframe_requests_sent),
            count(&stats.keyframe_requests_received),
        )
    }
}

/// Prints the peer's video once per [`REPORT_INTERVAL`] until the call ends.
async fn report(mut incoming: mpsc::Receiver<Frame>, mut keyframe_requests: mpsc::Receiver<()>, stats: &MediaStats) {
    let mut tick = tokio::time::interval_at(Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    let (mut window, mut since) = (Window::default(), Instant::now());
    loop {
        tokio::select! {
            frame = incoming.recv() => match frame {
                Some(frame) => window.add(&frame),
                None => break,
            },
            // A clip can't make keyframes on demand; its own keyframes answer. `stats` counts these.
            Some(()) = keyframe_requests.recv() => {}
            _ = tick.tick() => {
                println!("{}", window.line(since.elapsed().as_secs_f64(), stats));
                (window, since) = (Window::default(), Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LENGTH_SIZE: usize = 4;

    #[test]
    fn length_prefixes_become_start_codes() -> Result<()> {
        let sample = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x06];
        let config = [START_CODE.as_slice(), &[0x67], &START_CODE, &[0x68]].concat();
        let frame = [START_CODE.as_slice(), &[0x65, 0xaa], &START_CODE, &[0x06]].concat();
        assert_eq!(annex_b(&sample, LENGTH_SIZE, None)?, frame);
        assert_eq!(annex_b(&sample, LENGTH_SIZE, Some(&config))?, [config.as_slice(), &frame].concat());
        Ok(())
    }

    #[test]
    fn truncated_samples_are_rejected() {
        assert!(annex_b(&[0, 0, 0, 2, 0x65], LENGTH_SIZE, None).is_err());
        assert!(annex_b(&[0, 0], LENGTH_SIZE, None).is_err());
    }
}
