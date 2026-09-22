//! Reads an mp4 for playback into a call: the H.264 track's samples (sent as-is) and the AAC
//! track decoded once to 48 kHz mono PCM.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use mp4_atom::{Atom, Codec, Header, Moov, ReadAtom, ReadFrom, Stbl, StszSamples, Trak};
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AacDecoder;
use uplink_core::audio::SAMPLE_RATE;

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];
const QUARTER_TURN_DEGREES: u16 = 90;

/// Where a sample lives in the file and when it plays, in track timescale units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    pub decode_time: u64,
    pub composition_offset: i64,
    pub sync: bool,
}

pub struct VideoTrack {
    pub samples: Vec<Sample>,
    pub timescale: u32,
    /// Bytes of each NAL length prefix in the samples.
    pub length_size: usize,
    /// SPS + PPS in Annex-B form, prepended to every keyframe.
    pub config: Vec<u8>,
    pub turns: u8,
}

pub struct Clip {
    pub file: File,
    pub video: VideoTrack,
    /// The audio track at [`SAMPLE_RATE`] mono; empty without one.
    pub voice: Vec<i16>,
}

impl Clip {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let moov = read_moov(&mut file)?;
        let video = moov.trak.iter().find_map(|trak| video_track(trak).transpose()).context("no H.264 track")??;
        let voice = match moov.trak.iter().find(|trak| aac_entry(trak).is_some()) {
            Some(trak) => decode_aac(&file, trak)?,
            None => Vec::new(),
        };
        println!(
            "clip: {} video samples, rotated {}°, {:.1} s of audio",
            video.samples.len(),
            u16::from(video.turns) * QUARTER_TURN_DEGREES,
            f64::from(u32::try_from(voice.len()).unwrap_or(u32::MAX)) / f64::from(SAMPLE_RATE),
        );
        Ok(Self { file, video, voice })
    }

}

/// Skips top-level atoms (notably `mdat`) until the `moov`.
fn read_moov(file: &mut File) -> Result<Moov> {
    let end = file.metadata()?.len();
    while file.stream_position()? < end {
        let header = Header::read_from(file)?;
        if header.kind == Moov::KIND {
            return Ok(Moov::read_atom(&header, file)?);
        }
        let Some(size) = header.size else { break };
        file.seek(SeekFrom::Current(i64::try_from(size)?))?;
    }
    bail!("no moov atom")
}

fn video_track(trak: &Trak) -> Result<Option<VideoTrack>> {
    let stbl = &trak.mdia.minf.stbl;
    let Some(avc1) = stbl.stsd.codecs.iter().find_map(|c| if let Codec::Avc1(avc1) = c { Some(avc1) } else { None })
    else {
        return Ok(None);
    };
    let avcc = &avc1.avcc;
    let config = avcc
        .sequence_parameter_sets
        .iter()
        .chain(&avcc.picture_parameter_sets)
        .flat_map(|nal| [START_CODE.as_slice(), nal].concat())
        .collect();
    // Display rotation from the `tkhd` matrix (16.16 fixed point; the signs of a and b suffice).
    let matrix = &trak.tkhd.matrix;
    let turns = match (matrix.a.signum(), matrix.b.signum()) {
        (0, 1) => 1,
        (-1, 0) => 2,
        (0, -1) => 3,
        _ => 0,
    };
    Ok(Some(VideoTrack {
        samples: samples(stbl)?,
        timescale: trak.mdia.mdhd.timescale,
        length_size: usize::from(avcc.length_size),
        config,
        turns,
    }))
}

fn aac_entry(trak: &Trak) -> Option<&mp4_atom::Mp4a> {
    trak.mdia.minf.stbl.stsd.codecs.iter().find_map(|c| if let Codec::Mp4a(mp4a) = c { Some(mp4a) } else { None })
}

/// Walks the sample tables: sizes (`stsz`), chunk layout (`stsc` + `stco`/`co64`), decode times
/// (`stts`), composition offsets (`ctts`) and sync samples (`stss`).
pub fn samples(stbl: &Stbl) -> Result<Vec<Sample>> {
    let sizes = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => vec![*size; usize::try_from(*count)?],
        StszSamples::Different { sizes } => sizes.clone(),
    };
    let chunks: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().copied().map(u64::from).collect(),
        (None, Some(co64)) => co64.entries.clone(),
        (None, None) => bail!("no chunk offsets"),
    };
    let mut offsets = Vec::with_capacity(sizes.len());
    let mut size_iter = sizes.iter();
    for (index, &chunk_offset) in chunks.iter().enumerate() {
        let chunk = u32::try_from(index + 1)?;
        let per_chunk = stbl
            .stsc
            .entries
            .iter()
            .rev()
            .find(|entry| entry.first_chunk <= chunk)
            .map_or(0, |entry| entry.samples_per_chunk);
        let mut offset = chunk_offset;
        for size in size_iter.by_ref().take(usize::try_from(per_chunk)?) {
            offsets.push(offset);
            offset += u64::from(*size);
        }
    }
    ensure!(offsets.len() == sizes.len(), "chunk table covers {} of {} samples", offsets.len(), sizes.len());

    let count = |n: u32| usize::try_from(n).unwrap_or_default();
    let durations = stbl.stts.entries.iter().flat_map(|e| std::iter::repeat_n(e.sample_delta, count(e.sample_count)));
    let mut compositions = stbl
        .ctts
        .iter()
        .flat_map(|ctts| &ctts.entries)
        .flat_map(|e| std::iter::repeat_n(e.sample_offset, count(e.sample_count)));
    // `stss` lists 1-based sample numbers; without it every sample is a sync sample.
    let sync = |number: usize| {
        stbl.stss.as_ref().is_none_or(|stss| u32::try_from(number).is_ok_and(|n| stss.entries.binary_search(&n).is_ok()))
    };

    let mut decode_time = 0;
    let mut out = Vec::with_capacity(sizes.len());
    for (index, ((&size, &offset), duration)) in sizes.iter().zip(&offsets).zip(durations.chain(std::iter::repeat(0))).enumerate() {
        out.push(Sample {
            offset,
            size,
            decode_time,
            composition_offset: compositions.next().unwrap_or_default(),
            sync: sync(index + 1),
        });
        decode_time += u64::from(duration);
    }
    Ok(out)
}

/// Decodes an AAC track to [`SAMPLE_RATE`] mono.
fn decode_aac(file: &File, trak: &Trak) -> Result<Vec<i16>> {
    let Some(mp4a) = aac_entry(trak) else { return Ok(Vec::new()) };
    let rate = u32::from(mp4a.audio.sample_rate.integer());
    let channels = match mp4a.audio.channel_count {
        1 => Channels::FRONT_LEFT,
        2 => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
        n => bail!("{n}-channel AAC is not supported"),
    };
    let mut params = CodecParameters::new();
    params.for_codec(CODEC_TYPE_AAC).with_sample_rate(rate).with_channels(channels);
    if let Some(specific) = &mp4a.esds.es_desc.dec_config.dec_specific
        && !specific.raw.is_empty()
    {
        params.with_extra_data(specific.raw.clone().into_boxed_slice());
    }
    let mut decoder = AacDecoder::try_new(&params, &DecoderOptions::default())?;
    let mut mono = Vec::new();
    for sample in samples(&trak.mdia.minf.stbl)? {
        let mut data = vec![0; usize::try_from(sample.size)?];
        file.read_exact_at(&mut data, sample.offset)?;
        let decoded = decoder.decode(&Packet::new_from_slice(0, sample.decode_time, 0, &data))?;
        let spec = *decoded.spec();
        let mut buffer = SampleBuffer::<i16>::new(u64::try_from(decoded.capacity())?, spec);
        buffer.copy_interleaved_ref(decoded);
        mono.extend(downmix(buffer.samples(), spec.channels.count()));
    }
    Ok(resample(&mono, rate, u32::try_from(SAMPLE_RATE)?))
}

fn downmix(interleaved: &[i16], channels: usize) -> impl Iterator<Item = i16> + '_ {
    interleaved.chunks(channels.max(1)).map(|frame| {
        let sum: i32 = frame.iter().copied().map(i32::from).sum();
        let count = i32::try_from(frame.len()).unwrap_or(1).max(1);
        i16::try_from(sum / count).unwrap_or_default()
    })
}

/// Linear interpolation; plenty for a test source.
fn resample(input: &[i16], from: u32, to: u32) -> Vec<i16> {
    let Some(&last) = input.last() else { return Vec::new() };
    if from == to || from == 0 {
        return input.to_vec();
    }
    let (from, to) = (u64::from(from), u64::from(to));
    let len = u64::try_from(input.len()).unwrap_or(u64::MAX) * to / from;
    let at = |index: usize| i64::from(input.get(index).copied().unwrap_or(last));
    (0..len)
        .map(|i| {
            let position = i * from;
            let index = usize::try_from(position / to).unwrap_or(usize::MAX);
            let (a, b) = (at(index), at(index.saturating_add(1)));
            let (frac, to) = (i64::try_from(position % to).unwrap_or_default(), i64::try_from(to).unwrap_or(1));
            i16::try_from(a + (b - a) * frac / to).unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use mp4_atom::{Stco, Stsc, StscEntry, Stss, Stsz, Stts, SttsEntry};

    use super::*;

    const DELTA: u32 = 3000;

    #[test]
    fn sample_tables_resolve_offsets_times_and_sync() -> Result<()> {
        let stbl = Stbl {
            stsz: Stsz { samples: StszSamples::Different { sizes: vec![10, 20, 30, 40, 50] } },
            stco: Some(Stco { entries: vec![100, 1000] }),
            stsc: Stsc {
                entries: vec![
                    StscEntry { first_chunk: 1, samples_per_chunk: 3, sample_description_index: 1 },
                    StscEntry { first_chunk: 2, samples_per_chunk: 2, sample_description_index: 1 },
                ],
            },
            stts: Stts { entries: vec![SttsEntry { sample_count: 5, sample_delta: DELTA }] },
            stss: Some(Stss { entries: vec![1, 4] }),
            ..Stbl::default()
        };
        let samples = samples(&stbl)?;
        let offsets: Vec<u64> = samples.iter().map(|s| s.offset).collect();
        assert_eq!(offsets, [100, 110, 130, 1000, 1040]);
        assert_eq!(samples[2].decode_time, 2 * u64::from(DELTA));
        let sync: Vec<bool> = samples.iter().map(|s| s.sync).collect();
        assert_eq!(sync, [true, false, false, true, false]);
        Ok(())
    }

    #[test]
    fn short_chunk_tables_are_rejected() {
        let stbl = Stbl {
            stsz: Stsz { samples: StszSamples::Identical { count: 4, size: 8 } },
            stco: Some(Stco { entries: vec![0] }),
            stsc: Stsc { entries: vec![StscEntry { first_chunk: 1, samples_per_chunk: 2, sample_description_index: 1 }] },
            ..Stbl::default()
        };
        assert!(samples(&stbl).is_err());
    }

    #[test]
    fn stereo_downmixes_and_rates_convert() {
        let mono: Vec<i16> = downmix(&[100, 300, -50, -150], 2).collect();
        assert_eq!(mono, [200, -100]);
        assert_eq!(resample(&[0, 100], 24_000, 48_000), [0, 50, 100, 100]);
        assert_eq!(resample(&[7, 8], 48_000, 48_000), [7, 8]);
    }
}
