//! The pieces every Android binary resource format is built from: little-endian chunks with a
//! type/header-size/size prologue, pooled strings, and `Res_value` cells.
//!
//! Both the binary XML writer and the resource table are the same machinery over these, which is
//! the whole reason this tool can stand in for aapt2.

use std::collections::HashMap;

use anyhow::Result;

// ResourceTypes.h chunk types.
pub const RES_STRING_POOL_TYPE: u16 = 0x0001;
pub const RES_TABLE_TYPE: u16 = 0x0002;
pub const RES_XML_TYPE: u16 = 0x0003;
pub const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
pub const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
pub const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
pub const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
pub const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
pub const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
pub const RES_TABLE_TYPE_TYPE: u16 = 0x0201;
pub const RES_TABLE_TYPE_SPEC_TYPE: u16 = 0x0202;

pub const CHUNK_HEADER_SIZE: u16 = 8;
pub const STRING_POOL_HEADER_SIZE: u16 = 28;
pub const XML_NODE_HEADER_SIZE: u16 = 16;
pub const RES_VALUE_SIZE: u16 = 8;
pub const STRING_POOL_UTF8_FLAG: u32 = 1 << 8;
pub const NO_INDEX: u32 = u32::MAX;
pub const LINE_NUMBER: u32 = 1;
pub const CHUNK_ALIGN: usize = 4;
const UTF8_SHORT_LEN_MAX: usize = 0x7f;
const UTF8_LONG_LEN_FLAG: u8 = 0x80;

// Res_value data types.
pub const TYPE_REFERENCE: u8 = 0x01;
/// A reference to a theme attribute, as `?android:attr/foo` — resolved per theme at runtime.
pub const TYPE_ATTRIBUTE: u8 = 0x02;
pub const TYPE_STRING: u8 = 0x03;
pub const TYPE_INT_DEC: u8 = 0x10;
pub const TYPE_INT_HEX: u8 = 0x11;
pub const TYPE_INT_BOOLEAN: u8 = 0x12;
pub const TYPE_INT_COLOR_ARGB8: u8 = 0x1c;
pub const BOOL_TRUE: u32 = u32::MAX;

/// Deduplicated strings in insertion order, which is the order they are written in.
#[derive(Default)]
pub struct StringPool {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl StringPool {
    pub fn intern(&mut self, s: &str) -> Result<u32> {
        if let Some(&i) = self.index.get(s) {
            return Ok(i);
        }
        let i = u32::try_from(self.strings.len())?;
        self.strings.push(s.to_owned());
        self.index.insert(s.to_owned(), i);
        Ok(i)
    }

    /// The index of a string already interned; an error rather than a silent 0, since a wrong
    /// index in a resource table is a resource that resolves to the wrong thing.
    pub fn index_of(&self, s: &str) -> Result<u32> {
        self.index.get(s).copied().ok_or_else(|| anyhow::anyhow!("{s} was never interned"))
    }

    /// The pool as its own chunk: header, one offset per string, then the UTF-8 bodies.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut data = Writer::new();
        let mut offsets = Vec::with_capacity(self.strings.len());
        for s in &self.strings {
            offsets.push(data.len()?);
            data.utf8_len(s.chars().count())?;
            data.utf8_len(s.len())?;
            data.bytes(s.as_bytes());
            data.u8(0);
        }
        data.align();

        let strings_start = u32::from(STRING_POOL_HEADER_SIZE) + u32::try_from(offsets.len() * size_of::<u32>())?;
        let mut out = Writer::new();
        out.u16(RES_STRING_POOL_TYPE);
        out.u16(STRING_POOL_HEADER_SIZE);
        out.u32(strings_start + data.len()?);
        out.u32(u32::try_from(offsets.len())?);
        out.u32(0); // style count
        out.u32(STRING_POOL_UTF8_FLAG);
        out.u32(strings_start);
        out.u32(0); // styles start
        for o in offsets {
            out.u32(o);
        }
        out.bytes(&data.0);
        Ok(out.0)
    }
}

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub const fn new() -> Self {
        Self(Vec::new())
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    pub fn len(&self) -> Result<u32> {
        Ok(u32::try_from(self.0.len())?)
    }
    pub fn align(&mut self) {
        self.0.resize(self.0.len().next_multiple_of(CHUNK_ALIGN), 0);
    }
    /// A `Res_value` cell: size, padding, type, then the datum.
    pub fn value(&mut self, kind: u8, data: u32) {
        self.u16(RES_VALUE_SIZE);
        self.u8(0);
        self.u8(kind);
        self.u32(data);
    }
    pub fn node_header(&mut self, kind: u16, size: u32) {
        self.u16(kind);
        self.u16(XML_NODE_HEADER_SIZE);
        self.u32(size);
        self.u32(LINE_NUMBER);
        self.u32(NO_INDEX);
    }
    /// The pool's length prefix: one byte, or two with the high bit marking the long form.
    pub fn utf8_len(&mut self, n: usize) -> Result<()> {
        if n > UTF8_SHORT_LEN_MAX {
            let [hi, lo] = u16::try_from(n)?.to_be_bytes();
            self.u8(hi | UTF8_LONG_LEN_FLAG);
            self.u8(lo);
        } else {
            self.u8(u8::try_from(n)?);
        }
        Ok(())
    }
}
