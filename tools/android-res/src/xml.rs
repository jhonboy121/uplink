//! Binary XML: what Android reads instead of text for the manifest and for compiled resource XML
//! such as an adaptive icon.

use anyhow::Result;

use crate::chunk::{
    BOOL_TRUE, CHUNK_HEADER_SIZE, NO_INDEX, RES_VALUE_SIZE, RES_XML_END_ELEMENT_TYPE, RES_XML_END_NAMESPACE_TYPE,
    RES_XML_RESOURCE_MAP_TYPE, RES_XML_START_ELEMENT_TYPE, RES_XML_START_NAMESPACE_TYPE, RES_XML_TYPE, StringPool,
    TYPE_INT_BOOLEAN, TYPE_INT_DEC, TYPE_INT_HEX, TYPE_REFERENCE, TYPE_STRING, Writer,
};

pub const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";

const XML_NODE_SIZE: u32 = 24;
const ATTR_EXT_SIZE: u16 = 20;
const START_ELEMENT_SIZE: u32 = 36;

pub enum Value {
    Str(String),
    Int(u32),
    Hex(u32),
    Bool(bool),
    /// A resource id, as `@drawable/x` compiles to.
    Ref(u32),
}

pub struct Attr {
    /// `Some(resource id)` for android-namespaced attributes.
    pub res_id: Option<u32>,
    pub name: &'static str,
    pub value: Value,
}

pub struct Element {
    pub name: &'static str,
    pub attrs: Vec<Attr>,
    pub children: Vec<Element>,
}

fn collect_android_attrs(e: &Element, out: &mut Vec<(&'static str, u32)>) {
    for a in &e.attrs {
        if let Some(id) = a.res_id
            && !out.iter().any(|(n, _)| *n == a.name)
        {
            out.push((a.name, id));
        }
    }
    for c in &e.children {
        collect_android_attrs(c, out);
    }
}

fn intern_all(e: &Element, pool: &mut StringPool) -> Result<()> {
    pool.intern(e.name)?;
    for a in &e.attrs {
        pool.intern(a.name)?;
        if let Value::Str(s) = &a.value {
            pool.intern(s)?;
        }
    }
    e.children.iter().try_for_each(|c| intern_all(c, pool))
}

fn write_element(e: &Element, pool: &mut StringPool, ns: u32, w: &mut Writer) -> Result<()> {
    // Android-namespaced attributes sorted by resource id, then plain ones.
    let mut attrs: Vec<&Attr> = e.attrs.iter().collect();
    attrs.sort_by_key(|a| a.res_id.map_or((true, 0), |id| (false, id)));
    let count = u16::try_from(attrs.len())?;

    w.node_header(RES_XML_START_ELEMENT_TYPE, START_ELEMENT_SIZE + u32::from(ATTR_EXT_SIZE) * u32::from(count));
    w.u32(NO_INDEX);
    w.u32(pool.intern(e.name)?);
    w.u16(ATTR_EXT_SIZE);
    w.u16(ATTR_EXT_SIZE);
    w.u16(count);
    // id, class and style attribute indices: none.
    w.u16(0);
    w.u16(0);
    w.u16(0);
    for a in attrs {
        w.u32(if a.res_id.is_some() { ns } else { NO_INDEX });
        w.u32(pool.intern(a.name)?);
        let (raw, kind, data) = match &a.value {
            Value::Str(s) => {
                let i = pool.intern(s)?;
                (i, TYPE_STRING, i)
            }
            Value::Int(v) => (NO_INDEX, TYPE_INT_DEC, *v),
            Value::Hex(v) => (NO_INDEX, TYPE_INT_HEX, *v),
            Value::Bool(b) => (NO_INDEX, TYPE_INT_BOOLEAN, if *b { BOOL_TRUE } else { 0 }),
            Value::Ref(id) => (NO_INDEX, TYPE_REFERENCE, *id),
        };
        w.u32(raw);
        w.u16(RES_VALUE_SIZE);
        w.u8(0);
        w.u8(kind);
        w.u32(data);
    }
    for c in &e.children {
        write_element(c, pool, ns, w)?;
    }
    w.node_header(RES_XML_END_ELEMENT_TYPE, XML_NODE_SIZE);
    w.u32(NO_INDEX);
    w.u32(pool.intern(e.name)?);
    Ok(())
}

pub fn encode(root: &Element) -> Result<Vec<u8>> {
    let mut pool = StringPool::default();
    // Attribute names with resource ids must be the first strings in the pool.
    let mut android_attrs = Vec::new();
    collect_android_attrs(root, &mut android_attrs);
    for (name, _) in &android_attrs {
        pool.intern(name)?;
    }
    let prefix = pool.intern("android")?;
    let ns = pool.intern(ANDROID_NS)?;
    intern_all(root, &mut pool)?;

    let mut body = Writer::new();
    body.u16(RES_XML_RESOURCE_MAP_TYPE);
    body.u16(CHUNK_HEADER_SIZE);
    body.u32(u32::from(CHUNK_HEADER_SIZE) + u32::try_from(android_attrs.len() * size_of::<u32>())?);
    for (_, id) in &android_attrs {
        body.u32(*id);
    }
    body.node_header(RES_XML_START_NAMESPACE_TYPE, XML_NODE_SIZE);
    body.u32(prefix);
    body.u32(ns);
    write_element(root, &mut pool, ns, &mut body)?;
    body.node_header(RES_XML_END_NAMESPACE_TYPE, XML_NODE_SIZE);
    body.u32(prefix);
    body.u32(ns);

    let strings = pool.encode()?;
    let mut out = Writer::new();
    out.u16(RES_XML_TYPE);
    out.u16(CHUNK_HEADER_SIZE);
    out.u32(u32::from(CHUNK_HEADER_SIZE) + u32::try_from(strings.len())? + body.len()?);
    out.bytes(&strings);
    out.bytes(&body.0);
    Ok(out.0)
}
