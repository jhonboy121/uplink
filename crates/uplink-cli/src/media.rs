//! Call media for the CLI: plays a clip as our video and voice (video sent as-is, audio as
//! Opus), plays out the peer's audio, prints what the peer sends and optionally records it.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uplink_core::audio::{AudioReceiver, AudioSender, FRAME_DURATION, FRAME_SAMPLES};
use uplink_core::media::{Frame, MediaSession, MediaStats, VideoSender};

use crate::clip::{Clip, START_CODE, VideoTrack};
use crate::record::{FRAGMENT_INTERVAL, Recorder};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);
const MICROS_PER_SECOND: u64 = 1_000_000;
const BITS_PER_BYTE: f64 = 8.0;
const BITS_PER_KBIT: f64 = 1000.0;
const BYTES_PER_KIB: u32 = 1024;

/// Runs a call's media until the call ends (the peer's video channel closes).
pub async fn run(media: MediaSession, clip: Option<Clip>, recorder: Option<Recorder>) {
    let MediaSession { mut video, incoming_video, keyframe_requests, audio, incoming_audio, stats } = media;
    let (video_clip, voice) = match clip {
        Some(Clip { file, video, voice }) => (Some((file, video)), voice),
        None => (None, Vec::new()),
    };
    // Never completes: holding `video` keeps our side of the media alive for the whole call.
    let play = async move {
        if let Some((file, track)) = video_clip
            && let Err(e) = play_video(&file, &track, &mut video).await
        {
            println!("clip video stopped: {e:#}");
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        () = play => {}
        () = send_voice(voice, audio) => {}
        () = receive(incoming_video, incoming_audio, keyframe_requests, &stats, recorder) => {}
    }
}

fn duration(ticks: u64, timescale: u32) -> Duration {
    let micros = ticks.saturating_mul(MICROS_PER_SECOND).checked_div(u64::from(timescale)).unwrap_or_default();
    Duration::from_micros(micros)
}

/// Sends the clip's video in real time, looping; returns only on a read error.
async fn play_video(file: &File, track: &VideoTrack, video: &mut VideoSender) -> Result<()> {
    let start = Instant::now();
    let mut loop_offset = Duration::ZERO;
    loop {
        let mut end = 0;
        for sample in &track.samples {
            let mut data = vec![0; usize::try_from(sample.size)?];
            file.read_exact_at(&mut data, sample.offset)?;
            tokio::time::sleep_until(start + loop_offset + duration(sample.decode_time, track.timescale)).await;
            let presentation = sample.decode_time.saturating_add_signed(sample.composition_offset);
            let capture = loop_offset + duration(presentation, track.timescale);
            video.send(Frame {
                capture_micros: u64::try_from(capture.as_micros()).unwrap_or(u64::MAX),
                keyframe: sample.sync,
                config: false,
                turns: track.turns,
                data: annex_b(&data, track.length_size, sample.sync.then_some(track.config.as_slice()))?,
            });
            end = sample.decode_time;
        }
        // The last sample's duration isn't tracked; one frame's worth keeps the loop seamless.
        let frame = track.samples.get(1).map_or(0, |s| s.decode_time);
        loop_offset += duration(end + frame, track.timescale);
    }
}

/// Sends the clip's audio as 20 ms Opus frames in real time, looping; silent without audio.
async fn send_voice(voice: Vec<i16>, mut audio: AudioSender) {
    let (frames, _) = voice.as_chunks::<FRAME_SAMPLES>();
    if frames.is_empty() {
        return std::future::pending().await;
    }
    let start = Instant::now();
    let mut tick = tokio::time::interval(FRAME_DURATION);
    for frame in frames.iter().cycle() {
        tick.tick().await;
        let capture_micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Err(e) = audio.send(frame, capture_micros) {
            println!("clip audio stopped: {e:#}");
            break;
        }
    }
    std::future::pending().await
}

/// Length-prefixed NAL units (mp4 samples) → Annex-B, optionally led by `config`.
pub fn annex_b(sample: &[u8], length_size: usize, config: Option<&[u8]>) -> Result<Vec<u8>> {
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
            "rx {:.1} fps {:.0} kbps · {} key · max {} KiB · turns {} | video sent {} (late {}, congested {}) · \
             received {} (dropped {}) · keyframe asks sent {} received {} | audio sent {} received {} \
             (late {}, fec {}, concealed {})",
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
            count(&stats.audio_sent),
            count(&stats.audio_received),
            count(&stats.audio_late),
            count(&stats.audio_fec_recovered),
            count(&stats.audio_concealed),
        )
    }
}

/// Takes the peer's video and plays out its audio on a 20 ms clock (there is no speaker here;
/// playout drives the jitter buffer and the recording). Reports once per [`REPORT_INTERVAL`].
async fn receive(
    mut incoming_video: mpsc::Receiver<Frame>,
    mut incoming_audio: AudioReceiver,
    mut keyframe_requests: mpsc::Receiver<()>,
    stats: &MediaStats,
    mut recorder: Option<Recorder>,
) {
    let mut report = tokio::time::interval_at(Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    let mut flush = tokio::time::interval_at(Instant::now() + FRAGMENT_INTERVAL, FRAGMENT_INTERVAL);
    let mut playout = tokio::time::interval(FRAME_DURATION);
    let mut pcm = [0; FRAME_SAMPLES];
    let (mut window, mut since) = (Window::default(), Instant::now());
    loop {
        tokio::select! {
            frame = incoming_video.recv() => match frame {
                Some(frame) => {
                    window.add(&frame);
                    if let Some(recorder) = &mut recorder
                        && let Err(e) = recorder.video(&frame)
                    {
                        println!("recording video: {e:#}");
                    }
                }
                None => break,
            },
            _ = playout.tick() => match incoming_audio.next(&mut pcm) {
                Ok(Some((sequence, packet))) => {
                    if let Some(recorder) = &mut recorder {
                        recorder.audio(sequence, packet);
                    }
                }
                Ok(None) => {}
                Err(e) => println!("audio playout: {e:#}"),
            },
            // A clip can't make keyframes on demand; its own keyframes answer. `stats` counts these.
            Some(()) = keyframe_requests.recv() => {}
            _ = flush.tick() => {
                if let Some(recorder) = &mut recorder
                    && let Err(e) = recorder.flush()
                {
                    println!("recording: {e:#}");
                }
            }
            _ = report.tick() => {
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
