//! Binary AndroidManifest.xml (AXML) writer for NativeActivity apps; replaces aapt2, which only
//! ships for x86_64 hosts.
//!
//! usage: axml out=<path> package=<id> label=<name> lib=<libname> min=<sdk> target=<sdk>
//!             version=<code> [debuggable=<0|1>] [dex=<0|1>] [perm=<permission>]...

use std::collections::HashMap;
use anyhow::{Context, Result};

const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
const NATIVE_ACTIVITY: &str = "android.app.NativeActivity";
const LIB_NAME_META: &str = "android.app.lib_name";

// Resource ids from android.jar: `javap -constants 'android.R$attr'`.
const ATTR_LABEL: u32 = 0x0101_0001;
const ATTR_NAME: u32 = 0x0101_0003;
const ATTR_HAS_CODE: u32 = 0x0101_000c;
const ATTR_DEBUGGABLE: u32 = 0x0101_000f;
const ATTR_EXPORTED: u32 = 0x0101_0010;
const ATTR_LAUNCH_MODE: u32 = 0x0101_001d;
const ATTR_CONFIG_CHANGES: u32 = 0x0101_001f;
const ATTR_VALUE: u32 = 0x0101_0024;
const ATTR_MIN_SDK: u32 = 0x0101_020c;
const ATTR_VERSION_CODE: u32 = 0x0101_021b;
const ATTR_VERSION_NAME: u32 = 0x0101_021c;
const ATTR_WINDOW_SOFT_INPUT_MODE: u32 = 0x0101_022b;
const ATTR_TARGET_SDK: u32 = 0x0101_0270;
const ATTR_HARDWARE_ACCELERATED: u32 = 0x0101_02d3;

// ActivityInfo.CONFIG_* handled by the native side instead of restarting the activity.
const CONFIG_KEYBOARD: u32 = 0x0010;
const CONFIG_KEYBOARD_HIDDEN: u32 = 0x0020;
const CONFIG_ORIENTATION: u32 = 0x0080;
const CONFIG_SCREEN_LAYOUT: u32 = 0x0100;
const CONFIG_UI_MODE: u32 = 0x0200;
const CONFIG_SCREEN_SIZE: u32 = 0x0400;
const CONFIG_SMALLEST_SCREEN_SIZE: u32 = 0x0800;
const CONFIG_DENSITY: u32 = 0x1000;
const CONFIG_CHANGES: u32 = CONFIG_KEYBOARD
    | CONFIG_KEYBOARD_HIDDEN
    | CONFIG_ORIENTATION
    | CONFIG_SCREEN_LAYOUT
    | CONFIG_UI_MODE
    | CONFIG_SCREEN_SIZE
    | CONFIG_SMALLEST_SCREEN_SIZE
    | CONFIG_DENSITY;
const LAUNCH_MODE_SINGLE_TOP: u32 = 1;
const SOFT_INPUT_ADJUST_RESIZE: u32 = 0x10;

// ResourceTypes.h chunk types and header sizes.
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const CHUNK_HEADER_SIZE: u16 = 8;
const STRING_POOL_HEADER_SIZE: u16 = 28;
const XML_NODE_HEADER_SIZE: u16 = 16;
const XML_NODE_SIZE: u32 = 24;
const ATTR_EXT_SIZE: u16 = 20;
const START_ELEMENT_SIZE: u32 = 36;
const RES_VALUE_SIZE: u16 = 8;
const STRING_POOL_UTF8_FLAG: u32 = 1 << 8;
const NO_INDEX: u32 = u32::MAX;
const LINE_NUMBER: u32 = 1;
const UTF8_SHORT_LEN_MAX: usize = 0x7f;
const UTF8_LONG_LEN_FLAG: u8 = 0x80;
const CHUNK_ALIGN: usize = 4;

// Res_value data types.
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;
const BOOL_TRUE: u32 = u32::MAX;

enum Value {
    Str(String),
    Int(u32),
    Hex(u32),
    Bool(bool),
}

struct Attr {
    /// `Some(resource id)` for android-namespaced attributes.
    res_id: Option<u32>,
    name: &'static str,
    value: Value,
}

struct Element {
    name: &'static str,
    attrs: Vec<Attr>,
    children: Vec<Element>,
}

const fn android(res_id: u32, name: &'static str, value: Value) -> Attr {
    Attr { res_id: Some(res_id), name, value }
}

const fn element(name: &'static str, attrs: Vec<Attr>, children: Vec<Element>) -> Element {
    Element { name, attrs, children }
}

#[derive(Default)]
struct StringPool {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl StringPool {
    fn intern(&mut self, s: &str) -> Result<u32> {
        if let Some(&i) = self.index.get(s) {
            return Ok(i);
        }
        let i = u32::try_from(self.strings.len())?;
        self.strings.push(s.to_owned());
        self.index.insert(s.to_owned(), i);
        Ok(i)
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&self) -> Result<u32> {
        Ok(u32::try_from(self.0.len())?)
    }
    fn node_header(&mut self, kind: u16, size: u32) {
        self.u16(kind);
        self.u16(XML_NODE_HEADER_SIZE);
        self.u32(size);
        self.u32(LINE_NUMBER);
        self.u32(NO_INDEX);
    }
    fn utf8_len(&mut self, n: usize) -> Result<()> {
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

fn encode(root: &Element) -> Result<Vec<u8>> {
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

    let mut body = Writer(Vec::new());
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

    let mut data = Writer(Vec::new());
    let mut offsets = Vec::with_capacity(pool.strings.len());
    for s in &pool.strings {
        offsets.push(data.len()?);
        data.utf8_len(s.chars().count())?;
        data.utf8_len(s.len())?;
        data.0.extend_from_slice(s.as_bytes());
        data.u8(0);
    }
    data.0.resize(data.0.len().next_multiple_of(CHUNK_ALIGN), 0);

    let strings_start = u32::from(STRING_POOL_HEADER_SIZE) + u32::try_from(offsets.len() * size_of::<u32>())?;
    let mut out = Writer(Vec::new());
    let pool_size = strings_start + data.len()?;
    out.u16(RES_XML_TYPE);
    out.u16(CHUNK_HEADER_SIZE);
    out.u32(u32::from(CHUNK_HEADER_SIZE) + pool_size + body.len()?);
    out.u16(RES_STRING_POOL_TYPE);
    out.u16(STRING_POOL_HEADER_SIZE);
    out.u32(pool_size);
    out.u32(u32::try_from(offsets.len())?);
    out.u32(0); // style count
    out.u32(STRING_POOL_UTF8_FLAG);
    out.u32(strings_start);
    out.u32(0); // styles start
    for o in offsets {
        out.u32(o);
    }
    out.0.extend_from_slice(&data.0);
    out.0.extend_from_slice(&body.0);
    Ok(out.0)
}

struct Args {
    values: HashMap<String, String>,
    permissions: Vec<String>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut values = HashMap::new();
        let mut permissions = Vec::new();
        for arg in std::env::args().skip(1) {
            let (k, v) = arg.split_once('=').with_context(|| format!("expected key=value, got `{arg}`"))?;
            if k == "perm" {
                permissions.push(v.to_owned());
            } else {
                values.insert(k.to_owned(), v.to_owned());
            }
        }
        Ok(Self { values, permissions })
    }

    fn get(&self, key: &str) -> Result<String> {
        self.values.get(key).cloned().with_context(|| format!("missing {key}="))
    }

    fn num(&self, key: &str) -> Result<u32> {
        self.get(key)?.parse().with_context(|| format!("{key}= is not a number"))
    }

    fn flag(&self, key: &str) -> bool {
        self.values.get(key).is_some_and(|v| v == "1")
    }
}

fn manifest(args: &Args) -> Result<Element> {
    let version = args.num("version")?;
    let mut children = vec![element(
        "uses-sdk",
        vec![
            android(ATTR_MIN_SDK, "minSdkVersion", Value::Int(args.num("min")?)),
            android(ATTR_TARGET_SDK, "targetSdkVersion", Value::Int(args.num("target")?)),
        ],
        vec![],
    )];
    children.extend(
        args.permissions
            .iter()
            .map(|p| element("uses-permission", vec![android(ATTR_NAME, "name", Value::Str(p.clone()))], vec![])),
    );
    let activity = element(
        "activity",
        vec![
            android(ATTR_NAME, "name", Value::Str(NATIVE_ACTIVITY.into())),
            android(ATTR_EXPORTED, "exported", Value::Bool(true)),
            android(ATTR_CONFIG_CHANGES, "configChanges", Value::Hex(CONFIG_CHANGES)),
            android(ATTR_LAUNCH_MODE, "launchMode", Value::Int(LAUNCH_MODE_SINGLE_TOP)),
            android(ATTR_WINDOW_SOFT_INPUT_MODE, "windowSoftInputMode", Value::Hex(SOFT_INPUT_ADJUST_RESIZE)),
        ],
        vec![
            element(
                "meta-data",
                vec![
                    android(ATTR_NAME, "name", Value::Str(LIB_NAME_META.into())),
                    android(ATTR_VALUE, "value", Value::Str(args.get("lib")?)),
                ],
                vec![],
            ),
            element(
                "intent-filter",
                vec![],
                vec![
                    element("action", vec![android(ATTR_NAME, "name", Value::Str("android.intent.action.MAIN".into()))], vec![]),
                    element(
                        "category",
                        vec![android(ATTR_NAME, "name", Value::Str("android.intent.category.LAUNCHER".into()))],
                        vec![],
                    ),
                ],
            ),
        ],
    );
    children.push(element(
        "application",
        vec![
            android(ATTR_LABEL, "label", Value::Str(args.get("label")?)),
            android(ATTR_HAS_CODE, "hasCode", Value::Bool(args.flag("dex"))),
            android(ATTR_DEBUGGABLE, "debuggable", Value::Bool(args.flag("debuggable"))),
            android(ATTR_HARDWARE_ACCELERATED, "hardwareAccelerated", Value::Bool(true)),
        ],
        vec![activity],
    ));
    Ok(element(
        "manifest",
        vec![
            Attr { res_id: None, name: "package", value: Value::Str(args.get("package")?) },
            android(ATTR_VERSION_CODE, "versionCode", Value::Int(version)),
            android(ATTR_VERSION_NAME, "versionName", Value::Str(format!("0.{version}"))),
        ],
        children,
    ))
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    std::fs::write(args.get("out")?, encode(&manifest(&args)?)?)?;
    Ok(())
}
