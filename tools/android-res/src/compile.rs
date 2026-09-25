//! Turns real XML into Android's binary form: the same job aapt2 does, for the subset this app
//! needs.
//!
//! Two things have to be resolved on the way. `android:` attribute names become framework ids,
//! looked up in the generated table rather than copied by hand; and `@type/name` values become
//! resource ids, either ours from the symbol table or the framework's.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::chunk::{
    COMPLEX_MANTISSA_SHIFT, COMPLEX_UNIT_DIP, TYPE_ATTRIBUTE, TYPE_INT_BOOLEAN, TYPE_INT_DEC, TYPE_INT_HEX,
    TYPE_REFERENCE, TYPE_STRING,
};
use crate::framework;
use crate::xml::{ANDROID_NS, Attr, Element, Value};

/// Our own resources, `type/name` to id, built from the values file before anything referencing
/// them is compiled.
pub type Symbols = HashMap<String, u32>;

/// `${name}` in an attribute value, substituted from the command line — the few parts of a
/// manifest that are a build's business rather than the app's.
fn substitute(text: &str, defines: &HashMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find('}') else {
            bail!("unterminated ${{ in {text}");
        };
        let name = &tail[..end];
        let value = defines.get(name).with_context(|| format!("{text} needs --define {name}=..."))?;
        out.push_str(value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `@android:style/Theme.Foo`, `@color/ground`, or `@0x7f010000`.
fn reference(text: &str, symbols: &Symbols) -> Result<u32> {
    let body = text.trim_start_matches('@');
    if let Some(hex) = body.strip_prefix("0x") {
        return Ok(u32::from_str_radix(hex, 16)?);
    }
    if let Some(rest) = body.strip_prefix("android:") {
        let (kind, name) = rest.split_once('/').with_context(|| format!("{text} is not type/name"))?;
        let table = match kind {
            "attr" => framework::ATTRS,
            "style" => framework::STYLES,
            _ => bail!("{text}: only android attr and style are resolvable here"),
        };
        return framework::lookup(table, &name.replace('.', "_"))
            .with_context(|| format!("{text} is not in the framework table; run `just android-table`"));
    }
    symbols.get(body).copied().with_context(|| format!("{text} is not declared in the values file"))
}

/// Attribute values carry no type in XML, so the text decides: a reference, a boolean, a number,
/// otherwise a string.
fn value(text: &str, symbols: &Symbols) -> Result<Value> {
    if text.starts_with('@') {
        return Ok(Value::Ref(reference(text, symbols)?));
    }
    Ok(match text {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            if let Some(hex) = text.strip_prefix("0x") {
                Value::Hex(u32::from_str_radix(hex, 16)?)
            } else if let Ok(number) = text.parse::<u32>() {
                Value::Int(number)
            } else {
                Value::Str(text.to_owned())
            }
        }
    })
}

/// `android:` attributes whose format is not in their text. aapt2 knows these from the SDK's
/// `attrs.xml`; this is the part of it `<vector>` needs, which is all that uses them here.
const COLOR_ATTRS: [&str; 3] = ["fillColor", "strokeColor", "tint"];
const FLOAT_ATTRS: [&str; 14] = [
    "viewportWidth",
    "viewportHeight",
    "strokeWidth",
    "strokeAlpha",
    "strokeMiterLimit",
    "fillAlpha",
    "alpha",
    "translateX",
    "translateY",
    "scaleX",
    "scaleY",
    "rotation",
    "pivotX",
    "pivotY",
];
const DIMENSION_ATTRS: [&str; 2] = ["width", "height"];
/// Enum attributes and their values, as `attrs.xml` declares them (`Paint.Cap`, `Paint.Join`).
const ENUM_ATTRS: [(&str, &[(&str, u32)]); 3] = [
    ("strokeLineCap", &[("butt", 0), ("round", 1), ("square", 2)]),
    ("strokeLineJoin", &[("miter", 0), ("round", 1), ("bevel", 2)]),
    ("fillType", &[("nonZero", 0), ("evenOdd", 1)]),
];
const DP: &str = "dp";

/// The value of an `android:` attribute whose format decides it, or `None` to type it by its text.
fn typed(name: &str, text: &str) -> Result<Option<Value>> {
    if COLOR_ATTRS.contains(&name) && text.starts_with('#') {
        return Ok(Some(Value::Color(color(text)?)));
    }
    if FLOAT_ATTRS.contains(&name) {
        return Ok(Some(Value::Float(text.parse().with_context(|| format!("android:{name}={text} is not a number"))?)));
    }
    if DIMENSION_ATTRS.contains(&name) {
        let whole: u32 = text
            .strip_suffix(DP)
            .and_then(|n| n.parse().ok())
            .with_context(|| format!("android:{name}={text}: only whole dp is supported"))?;
        return Ok(Some(Value::Dimension((whole << COMPLEX_MANTISSA_SHIFT) | COMPLEX_UNIT_DIP)));
    }
    if let Some((_, values)) = ENUM_ATTRS.iter().find(|(attr, _)| *attr == name) {
        let (_, number) = values
            .iter()
            .find(|(word, _)| *word == text)
            .with_context(|| format!("android:{name}={text} is not one of its values"))?;
        return Ok(Some(Value::Int(*number)));
    }
    Ok(None)
}

/// Element and attribute names outlive the document they were parsed from, so they are leaked
/// deliberately: this is a short-lived tool and the alternative is threading a lifetime through
/// the writer for no gain.
fn name_of(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

pub fn element(node: roxmltree::Node, symbols: &Symbols, defines: &HashMap<String, String>) -> Result<Element> {
    let mut attrs = Vec::new();
    for attr in node.attributes() {
        let text = substitute(attr.value(), defines)?;
        let res_id = match attr.namespace() {
            Some(ANDROID_NS) => Some(
                framework::lookup(framework::ATTRS, attr.name())
                    .with_context(|| format!("android:{} is not a framework attribute", attr.name()))?,
            ),
            Some(other) => bail!("unknown namespace {other} on {}", attr.name()),
            None => None,
        };
        let typed = if res_id.is_some() { typed(attr.name(), &text)? } else { None };
        let value = match typed {
            Some(value) => value,
            None => value(&text, symbols)?,
        };
        attrs.push(Attr { res_id, name: name_of(attr.name()), value });
    }
    let children = node
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|child| element(child, symbols, defines))
        .collect::<Result<Vec<_>>>()?;
    Ok(Element { name: name_of(node.tag_name().name()), attrs, children })
}

pub fn document(text: &str, symbols: &Symbols, defines: &HashMap<String, String>) -> Result<Element> {
    let parsed = roxmltree::Document::parse(text)?;
    element(parsed.root_element(), symbols, defines)
}

/// A colour literal as `#AARRGGBB` or `#RRGGBB`.
pub fn color(text: &str) -> Result<u32> {
    let body = text.trim_start_matches('#');
    let packed = u32::from_str_radix(body, 16)?;
    Ok(match body.len() {
        6 => 0xff00_0000 | packed,
        8 => packed,
        _ => bail!("{text} is not #RRGGBB or #AARRGGBB"),
    })
}

/// A `<string>`'s text as aapt2 reads it: runs of whitespace fold to one space, and `\'`, `\"`,
/// `\\`, `\n`, `\t`, `\@` and `\?` stand for themselves (or a newline, a tab). A literal `@` or
/// `?` would otherwise start a reference.
pub fn string_text(node: roxmltree::Node) -> Result<String> {
    let raw: String = node.descendants().filter(roxmltree::Node::is_text).filter_map(|n| n.text()).collect();
    let folded = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(folded.len());
    let mut chars = folded.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(escaped @ ('\'' | '"' | '\\' | '@' | '?')) => out.push(escaped),
            other => bail!("unknown escape \\{} in {folded}", other.map(String::from).unwrap_or_default()),
        }
    }
    if out.starts_with(['@', '?']) {
        bail!("{out}: a string starting with @ or ? must escape it");
    }
    Ok(out)
}

/// A `<plurals>`: `<item quantity="one">…</item>` for each quantity the language has.
pub fn plural(node: roxmltree::Node) -> Result<Vec<(u32, String)>> {
    use crate::arsc::{ATTR_FEW, ATTR_MANY, ATTR_ONE, ATTR_OTHER, ATTR_TWO, ATTR_ZERO};
    let name = node.attribute("name").unwrap_or_default();
    let items = node
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|item| {
            let quantity = match item.attribute("quantity") {
                Some("zero") => ATTR_ZERO,
                Some("one") => ATTR_ONE,
                Some("two") => ATTR_TWO,
                Some("few") => ATTR_FEW,
                Some("many") => ATTR_MANY,
                Some("other") => ATTR_OTHER,
                other => bail!("plurals {name}: {other:?} is not a quantity"),
            };
            Ok((quantity, string_text(item)?))
        })
        .collect::<Result<Vec<_>>>()?;
    // `other` is what every language falls back to, so a plural without one can come out empty.
    if !items.iter().any(|(quantity, _)| *quantity == ATTR_OTHER) {
        bail!("plurals {name} needs an \"other\" item");
    }
    Ok(items)
}

/// A style's `parent="@android:style/Theme.Foo"`.
pub fn style_parent(text: &str, symbols: &Symbols) -> Result<u32> {
    reference(text, symbols).with_context(|| format!("style parent {text}"))
}

/// One item of a style, as `<item name="android:windowBackground">@color/ground</item>`.
pub fn style_item(node: roxmltree::Node, symbols: &Symbols) -> Result<(u32, u8, u32)> {
    // An item names an attribute directly — `android:windowBackground`, not a `type/name` pair.
    let name = node.attribute("name").context("a style item needs a name")?;
    let attr = match name.strip_prefix("android:") {
        Some(bare) => framework::lookup(framework::ATTRS, bare)
            .with_context(|| format!("android:{bare} is not a framework attribute"))?,
        None => bail!("{name}: only android: attributes can be set in a style here"),
    };
    let text = node.text().unwrap_or_default().trim();
    // `?android:attr/foo` defers to the theme, which is how a value follows light and dark
    // without a second configuration in the table.
    if let Some(deferred) = text.strip_prefix('?') {
        return Ok((attr, TYPE_ATTRIBUTE, reference(&format!("@{deferred}"), symbols)?));
    }
    Ok(match value(text, symbols)? {
        Value::Ref(id) => (attr, TYPE_REFERENCE, id),
        Value::Bool(b) => (attr, TYPE_INT_BOOLEAN, u32::from(b) * u32::MAX),
        Value::Int(n) => (attr, TYPE_INT_DEC, n),
        Value::Hex(n) => (attr, TYPE_INT_HEX, n),
        Value::Str(_) => (attr, TYPE_STRING, 0),
        // Only typed by attribute on an element (`typed`); no style here sets one.
        Value::Color(_) | Value::Float(_) | Value::Dimension(_) => bail!("{name}: not a value a style takes here"),
    })
}
