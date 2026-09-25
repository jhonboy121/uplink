//! Writes the APK zip.
//!
//! `jar` cannot do what Android 11 and above require: from API 30 an APK is refused unless its
//! `resources.arsc` is **stored uncompressed and aligned to four bytes**. `zipalign` would fix
//! that, but it is an x86-64 binary like aapt2, so the zip is written here instead — deflating
//! everything except the table, which is stored and padded into alignment.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::{Compression, Crc, write::DeflateEncoder};

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_DIRECTORY: u32 = 0x0605_4b50;
const LOCAL_HEADER_SIZE: usize = 30;
const CENTRAL_ENTRY_SIZE: usize = 46;
const END_SIZE: usize = 22;
const VERSION_NEEDED: u16 = 20;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
/// zipalign's own extra-field id for the padding it inserts.
const ALIGNMENT_EXTRA_ID: u16 = 0xd935;
const EXTRA_HEADER_SIZE: usize = 4;

/// What API 30+ demands of the resource table, and so what it is aligned to.
pub const ARSC_ALIGNMENT: usize = 4;
pub const ARSC_NAME: &str = "resources.arsc";

struct Entry {
    name: String,
    crc: u32,
    stored: Vec<u8>,
    method: u16,
    original: usize,
    offset: u32,
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Padding for this entry's extra field so its data begins on an `align` boundary. An extra
/// field cannot be 1..3 bytes long — it needs its own four-byte header — so the next multiple up
/// is taken instead.
const fn padding(base: usize, align: usize) -> usize {
    if align <= 1 {
        return 0;
    }
    let short = (align - base % align) % align;
    if short == 0 {
        0
    } else if short < EXTRA_HEADER_SIZE {
        short + align * EXTRA_HEADER_SIZE.div_ceil(align)
    } else {
        short
    }
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Files in the order given; `resources.arsc` is stored and aligned, everything else deflated.
pub fn write(out_path: &Path, root: &Path, files: &[PathBuf]) -> Result<()> {
    let mut body: Vec<u8> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();

    for path in files {
        let name = path
            .strip_prefix(root)?
            .to_str()
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?
            .replace('\\', "/");
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut crc = Crc::new();
        crc.update(&data);

        let table = name == ARSC_NAME;
        let (method, stored) = if table { (METHOD_STORED, data.clone()) } else { (METHOD_DEFLATE, deflate(&data)?) };

        let offset = u32::try_from(body.len())?;
        let pad = if table { padding(body.len() + LOCAL_HEADER_SIZE + name.len(), ARSC_ALIGNMENT) } else { 0 };

        put32(&mut body, LOCAL_HEADER);
        put16(&mut body, VERSION_NEEDED);
        put16(&mut body, 0); // flags
        put16(&mut body, method);
        put16(&mut body, 0); // time
        put16(&mut body, 0); // date
        put32(&mut body, crc.sum());
        put32(&mut body, u32::try_from(stored.len())?);
        put32(&mut body, u32::try_from(data.len())?);
        put16(&mut body, u16::try_from(name.len())?);
        put16(&mut body, u16::try_from(pad)?);
        body.extend_from_slice(name.as_bytes());
        if pad > 0 {
            put16(&mut body, ALIGNMENT_EXTRA_ID);
            put16(&mut body, u16::try_from(pad - EXTRA_HEADER_SIZE)?);
            body.resize(body.len() + pad - EXTRA_HEADER_SIZE, 0);
        }
        if table {
            debug_assert_eq!(body.len() % ARSC_ALIGNMENT, 0, "the table must start aligned");
        }
        body.extend_from_slice(&stored);

        entries.push(Entry { name, crc: crc.sum(), stored, method, original: data.len(), offset });
    }

    let directory_offset = u32::try_from(body.len())?;
    let mut directory: Vec<u8> = Vec::new();
    for entry in &entries {
        put32(&mut directory, CENTRAL_HEADER);
        put16(&mut directory, VERSION_NEEDED); // made by
        put16(&mut directory, VERSION_NEEDED);
        put16(&mut directory, 0); // flags
        put16(&mut directory, entry.method);
        put16(&mut directory, 0); // time
        put16(&mut directory, 0); // date
        put32(&mut directory, entry.crc);
        put32(&mut directory, u32::try_from(entry.stored.len())?);
        put32(&mut directory, u32::try_from(entry.original)?);
        put16(&mut directory, u16::try_from(entry.name.len())?);
        put16(&mut directory, 0); // extra length: the central copy needs none
        put16(&mut directory, 0); // comment
        put16(&mut directory, 0); // disk
        put16(&mut directory, 0); // internal attributes
        put32(&mut directory, 0); // external attributes
        put32(&mut directory, entry.offset);
        directory.extend_from_slice(entry.name.as_bytes());
    }

    let mut end: Vec<u8> = Vec::new();
    put32(&mut end, END_OF_DIRECTORY);
    put16(&mut end, 0); // disk
    put16(&mut end, 0); // directory start disk
    put16(&mut end, u16::try_from(entries.len())?);
    put16(&mut end, u16::try_from(entries.len())?);
    put32(&mut end, u32::try_from(directory.len())?);
    put32(&mut end, directory_offset);
    put16(&mut end, 0); // comment

    debug_assert_eq!(end.len(), END_SIZE);
    debug_assert_eq!(directory.len(), entries.iter().map(|e| CENTRAL_ENTRY_SIZE + e.name.len()).sum::<usize>());

    let mut file = body;
    file.extend_from_slice(&directory);
    file.extend_from_slice(&end);
    std::fs::write(out_path, file)?;
    Ok(())
}
