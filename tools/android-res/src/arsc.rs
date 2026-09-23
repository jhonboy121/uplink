//! Writes `resources.arsc`, the table `android:icon` and a theme are references into.
//!
//! Only what this app needs: one package, one (default) configuration per type, simple entries
//! for colours and file references, and map entries for a style. minSdk 30 is what keeps it that
//! small — adaptive icons exist on every device we support, so there are no density variants and
//! no `-v26` qualifier to encode.

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

/// What a resource resolves to.
pub enum Res {
    /// An inline datum: a colour, a boolean, a number.
    Value { kind: u8, data: u32 },
    /// A path inside the APK, interned in the table's own string pool.
    File(String),
    /// A style: framework attribute ids to values.
    Map { parent: u32, items: Vec<(u32, u8, u32)> },
}

pub struct Entry {
    pub name: String,
    pub res: Res,
}

/// One resource type — `color`, `drawable`, `mipmap`, `style` — and everything under it.
pub struct Type {
    pub name: &'static str,
    pub entries: Vec<Entry>,
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
            if let Res::File(path) = &entry.res {
                values.intern(path)?;
            }
        }
    }

    let mut body = Writer::new();
    for (index, kind) in types.iter().enumerate() {
        body.bytes(&type_spec(index, kind)?);
        body.bytes(&type_chunk(index, kind, &mut values, &keys)?);
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

/// Which configurations each entry varies by. Everything here is default-only, so the flags are
/// zero, but the chunk itself is not optional.
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
    for _ in 0..count {
        w.u32(0);
    }
    Ok(w.0)
}

fn type_chunk(index: usize, kind: &Type, values: &mut StringPool, keys: &StringPool) -> Result<Vec<u8>> {
    let count = u32::try_from(kind.entries.len())?;
    let mut offsets = Writer::new();
    let mut data = Writer::new();
    for entry in &kind.entries {
        offsets.u32(data.len()?);
        let key = keys.index_of(&entry.name)?;
        match &entry.res {
            Res::Value { kind, data: datum } => {
                data.u16(ENTRY_SIZE);
                data.u16(0);
                data.u32(key);
                data.value(*kind, *datum);
            }
            Res::File(path) => {
                data.u16(ENTRY_SIZE);
                data.u16(0);
                data.u32(key);
                data.value(crate::chunk::TYPE_STRING, values.intern(path)?);
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
    // The default configuration: every selector zero, so it matches anything.
    w.u32(u32::try_from(CONFIG_SIZE)?);
    w.bytes(&vec![0; CONFIG_SIZE - size_of::<u32>()]);
    w.bytes(&offsets.0);
    w.bytes(&data.0);
    Ok(w.0)
}

const _: () = assert!(CHUNK_HEADER_SIZE == 8, "chunk prologue is fixed by the format");
