//! Writes `resources.arsc`, the table `android:icon` and a theme are references into.
//!
//! Only what this app needs: one package, simple entries for colours, strings and file
//! references, and map entries for a style or a plural. The default configuration, plus one per
//! language a `values-<lang>/` folder translates. minSdk 30 is what keeps it that small — adaptive
//! icons exist on every device we support, so there are no density variants and no `-v26`
//! qualifier to encode.

use anyhow::{Result, bail};

use crate::chunk::{
    CHUNK_HEADER_SIZE, RES_TABLE_PACKAGE_TYPE, RES_TABLE_TYPE, RES_TABLE_TYPE_SPEC_TYPE, RES_TABLE_TYPE_TYPE,
    StringPool, Writer,
};

/// Application packages are 0x7f; the framework is 0x01.
const PACKAGE_ID: u32 = 0x7f;
/// `ResTable_package` carries a fixed 128-unit UTF-16 name.
const PACKAGE_NAME_UNITS: usize = 128;
const PACKAGE_HEADER_SIZE: u16 = 288;
const TABLE_HEADER_SIZE: u16 = 12;
const TYPE_SPEC_HEADER_SIZE: u16 = 16;
/// `ResTable_type` is the 20-byte header plus the configuration inline.
const TYPE_HEADER_SIZE: u16 = 20 + CONFIG_SIZE as u16;
const CONFIG_SIZE: usize = 64;
const ENTRY_SIZE: u16 = 8;
const MAP_ENTRY_SIZE: u16 = 16;
const ENTRY_FLAG_COMPLEX: u16 = 0x0001;
/// An entry a configuration does not have: the lookup falls back to the default one.
const NO_ENTRY: u32 = u32::MAX;
/// `ResTable_config`'s locale selector, as a type spec flags an entry that varies by it.
const CONFIG_LOCALE: u32 = 0x0004;
/// Where `ResTable_config.language` sits: after `size`, `mcc` and `mnc`.
const CONFIG_LANGUAGE_OFFSET: usize = 8;

// ResourceTypes.h `ResTable_map` names for a plural's quantities: `Res_MAKEINTERNAL(4..=9)`.
pub const ATTR_OTHER: u32 = 0x0100_0004;
pub const ATTR_ZERO: u32 = 0x0100_0005;
pub const ATTR_ONE: u32 = 0x0100_0006;
pub const ATTR_TWO: u32 = 0x0100_0007;
pub const ATTR_FEW: u32 = 0x0100_0008;
pub const ATTR_MANY: u32 = 0x0100_0009;

/// What a resource resolves to.
#[derive(Clone)]
pub enum Res {
    /// An inline datum: a colour, a boolean, a number.
    Value { kind: u8, data: u32 },
    /// A path inside the APK, interned in the table's own string pool.
    File(String),
    /// Text, interned in the same pool.
    Str(String),
    /// A style: framework attribute ids to values.
    Map { parent: u32, items: Vec<(u32, u8, u32)> },
    /// A plural: one string per quantity the language distinguishes, keyed by `ATTR_*`.
    Plural(Vec<(u32, String)>),
}

impl Res {
    /// Every string this puts in the value pool.
    fn strings(&self) -> Vec<&str> {
        match self {
            Self::File(text) | Self::Str(text) => vec![text],
            Self::Plural(items) => items.iter().map(|(_, text)| text.as_str()).collect(),
            Self::Value { .. } | Self::Map { .. } => Vec::new(),
        }
    }
}

pub struct Entry {
    pub name: String,
    pub res: Res,
}

/// A type's entries in one language: aligned with the default entries by index, `None` where
/// this language has no translation of its own.
pub struct Localized {
    /// ISO 639-1, as the `values-<lang>` folder names it.
    pub language: [u8; 2],
    pub entries: Vec<Option<Res>>,
}

/// One resource type — `color`, `drawable`, `mipmap`, `style`, `string`, `plurals` — and
/// everything under it.
pub struct Type {
    pub name: &'static str,
    pub entries: Vec<Entry>,
    pub localized: Vec<Localized>,
}

impl Type {
    /// The id a reference to `entries[index]` uses: package, then type, then entry.
    pub const fn id(type_index: usize, entry_index: usize) -> u32 {
        (PACKAGE_ID << 24) | (((type_index as u32) + 1) << 16) | (entry_index as u32)
    }
}

pub fn encode(package: &str, types: &[Type]) -> Result<Vec<u8>> {
    if types.is_empty() {
        bail!("a resource table needs at least one type");
    }
    // The value pool holds file paths; type and key names get their own pools, as the format
    // addresses them by separate indices.
    let mut values = StringPool::default();
    let mut type_names = StringPool::default();
    let mut keys = StringPool::default();
    for kind in types {
        type_names.intern(kind.name)?;
        for entry in &kind.entries {
            keys.intern(&entry.name)?;
            for text in entry.res.strings() {
                values.intern(text)?;
            }
        }
        for localized in &kind.localized {
            for text in localized.entries.iter().flatten().flat_map(Res::strings) {
                values.intern(text)?;
            }
        }
    }

    let mut body = Writer::new();
    for (index, kind) in types.iter().enumerate() {
        body.bytes(&type_spec(index, kind)?);
        let defaults: Vec<(&str, Option<&Res>)> =
            kind.entries.iter().map(|entry| (entry.name.as_str(), Some(&entry.res))).collect();
        body.bytes(&type_chunk(index, &defaults, None, &values, &keys)?);
        for localized in &kind.localized {
            let entries: Vec<(&str, Option<&Res>)> = kind
                .entries
                .iter()
                .zip(&localized.entries)
                .map(|(entry, res)| (entry.name.as_str(), res.as_ref()))
                .collect();
            body.bytes(&type_chunk(index, &entries, Some(localized.language), &values, &keys)?);
        }
    }

    let type_pool = type_names.encode()?;
    let key_pool = keys.encode()?;
    let mut package_chunk = Writer::new();
    package_chunk.u16(RES_TABLE_PACKAGE_TYPE);
    package_chunk.u16(PACKAGE_HEADER_SIZE);
    package_chunk.u32(
        u32::from(PACKAGE_HEADER_SIZE)
            + u32::try_from(type_pool.len())?
            + u32::try_from(key_pool.len())?
            + body.len()?,
    );
    package_chunk.u32(PACKAGE_ID);
    let mut name: Vec<u16> = package.encode_utf16().collect();
    if name.len() >= PACKAGE_NAME_UNITS {
        bail!("package name is too long for the table: {package}");
    }
    name.resize(PACKAGE_NAME_UNITS, 0);
    for unit in name {
        package_chunk.u16(unit);
    }
    package_chunk.u32(u32::from(PACKAGE_HEADER_SIZE)); // type strings offset
    package_chunk.u32(0); // last public type
    package_chunk.u32(u32::from(PACKAGE_HEADER_SIZE) + u32::try_from(type_pool.len())?); // key strings
    package_chunk.u32(0); // last public key
    package_chunk.u32(0); // type id offset
    package_chunk.bytes(&type_pool);
    package_chunk.bytes(&key_pool);
    package_chunk.bytes(&body.0);

    // The value pool is written before the package but interned while writing it, so it is
    // encoded last and prepended here.
    let value_pool = values.encode()?;
    let mut out = Writer::new();
    out.u16(RES_TABLE_TYPE);
    out.u16(TABLE_HEADER_SIZE);
    out.u32(u32::from(TABLE_HEADER_SIZE) + u32::try_from(value_pool.len())? + package_chunk.len()?);
    out.u32(1); // package count
    out.bytes(&value_pool);
    out.bytes(&package_chunk.0);
    Ok(out.0)
}

/// Which configurations each entry varies by: the locale, for an entry some language translates.
fn type_spec(index: usize, kind: &Type) -> Result<Vec<u8>> {
    let count = u32::try_from(kind.entries.len())?;
    let mut w = Writer::new();
    w.u16(RES_TABLE_TYPE_SPEC_TYPE);
    w.u16(TYPE_SPEC_HEADER_SIZE);
    w.u32(u32::from(TYPE_SPEC_HEADER_SIZE) + count * u32::try_from(size_of::<u32>())?);
    w.u8(u8::try_from(index + 1)?);
    w.u8(0); // res0
    w.u16(0); // res1
    w.u32(count);
    for entry in 0..kind.entries.len() {
        let translated = kind.localized.iter().any(|localized| localized.entries.get(entry).is_some_and(Option::is_some));
        w.u32(if translated { CONFIG_LOCALE } else { 0 });
    }
    Ok(w.0)
}

/// One configuration of a type: the default when `language` is `None`.
fn type_chunk(
    index: usize,
    entries: &[(&str, Option<&Res>)],
    language: Option<[u8; 2]>,
    values: &StringPool,
    keys: &StringPool,
) -> Result<Vec<u8>> {
    let count = u32::try_from(entries.len())?;
    let mut offsets = Writer::new();
    let mut data = Writer::new();
    for (name, res) in entries {
        let Some(res) = res else {
            offsets.u32(NO_ENTRY);
            continue;
        };
        offsets.u32(data.len()?);
        let key = keys.index_of(name)?;
        match res {
            Res::Value { kind, data: datum } => {
                data.u16(ENTRY_SIZE);
                data.u16(0);
                data.u32(key);
                data.value(*kind, *datum);
            }
            Res::File(text) | Res::Str(text) => {
                data.u16(ENTRY_SIZE);
                data.u16(0);
                data.u32(key);
                data.value(crate::chunk::TYPE_STRING, values.index_of(text)?);
            }
            Res::Map { parent, items } => {
                data.u16(MAP_ENTRY_SIZE);
                data.u16(ENTRY_FLAG_COMPLEX);
                data.u32(key);
                data.u32(*parent);
                data.u32(u32::try_from(items.len())?);
                for (attr, kind, datum) in items {
                    data.u32(*attr);
                    data.value(*kind, *datum);
                }
            }
            Res::Plural(items) => {
                data.u16(MAP_ENTRY_SIZE);
                data.u16(ENTRY_FLAG_COMPLEX);
                data.u32(key);
                data.u32(0); // no parent
                data.u32(u32::try_from(items.len())?);
                for (quantity, text) in items {
                    data.u32(*quantity);
                    data.value(crate::chunk::TYPE_STRING, values.index_of(text)?);
                }
            }
        }
    }

    let entries_start = u32::from(TYPE_HEADER_SIZE) + count * u32::try_from(size_of::<u32>())?;
    let mut w = Writer::new();
    w.u16(RES_TABLE_TYPE_TYPE);
    w.u16(TYPE_HEADER_SIZE);
    w.u32(entries_start + data.len()?);
    w.u8(u8::try_from(index + 1)?);
    w.u8(0); // flags: not sparse
    w.u16(0); // reserved
    w.u32(count);
    w.u32(entries_start);
    // Every selector zero matches anything; a language sets only its two letters.
    let mut config = vec![0; CONFIG_SIZE];
    config[..size_of::<u32>()].copy_from_slice(&u32::try_from(CONFIG_SIZE)?.to_le_bytes());
    if let Some(language) = language {
        config[CONFIG_LANGUAGE_OFFSET..CONFIG_LANGUAGE_OFFSET + language.len()].copy_from_slice(&language);
    }
    w.bytes(&config);
    w.bytes(&offsets.0);
    w.bytes(&data.0);
    Ok(w.0)
}

const _: () = assert!(CHUNK_HEADER_SIZE == 8, "chunk prologue is fixed by the format");
