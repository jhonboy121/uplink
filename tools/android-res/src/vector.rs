//! The UI's SVG icons as Android `<vector>` drawables, so the notification, the launcher icon and
//! the picture-in-picture buttons draw from the same source as the window, at any density, with
//! no rasterised copies to keep in step.
//!
//! Only what these SVGs use is understood: `<path>`, `<circle>` (as two arcs), fill and stroke with
//! their width, caps and joins, inherited from `<svg>`. Anything else is an error, not a guess.

use std::path::Path;

use anyhow::{Context, Result, bail};
use xmlwriter::{Indent, Options, XmlWriter};

use crate::xml::ANDROID_NS;

/// A plain icon is drawn at the size Android asks for one: a notification's small icon and a
/// remote action are both 24dp.
const ICON_DP: f64 = 24.0;
/// An adaptive icon's layer, and the part of it the launcher's mask always keeps is the middle
/// 72dp; the mark sits inside that at 60dp, as the launcher icon has always had it.
const ADAPTIVE_DP: f64 = 108.0;
const MARK_DP: f64 = 60.0;
/// The mark's own bounds in its 48-unit viewBox (the circles' outer edges and the arc's stroke),
/// for the notification icon, which is drawn flat and should fill its box.
const MARK_ART: Rect = Rect { x: 8.4, y: 4.4, width: 31.2, height: 34.35 };
const WHITE: &str = "#FFFFFFFF";
/// SVG's default fill, for a shape that names none anywhere up its tree.
const SVG_DEFAULT_FILL: &str = "#000000";
const HALF: f64 = 2.0;
const DP: &str = "dp";
/// Spaces per level, for elements and for attributes on their own lines, as Android's own XML is.
const XML_INDENT: u8 = 4;

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy)]
enum Placement {
    /// Its own viewBox, at [`ICON_DP`].
    Icon,
    /// The whole mark centred at [`MARK_DP`] in an [`ADAPTIVE_DP`] layer.
    Adaptive,
    /// Cropped to [`MARK_ART`], centred in a square [`ICON_DP`] box.
    Cropped,
}

#[derive(Clone, Copy)]
enum Paint {
    AsDrawn,
    /// Every colour white: for what the system tints or draws from alpha alone.
    White,
}

/// Source SVG, drawable name, and how it is placed and painted.
const TARGETS: [(&str, &str, Placement, Paint); 11] = [
    ("mic.svg", "mic", Placement::Icon, Paint::AsDrawn),
    ("mic-off.svg", "mic_off", Placement::Icon, Paint::AsDrawn),
    ("call-end.svg", "call_end", Placement::Icon, Paint::AsDrawn),
    ("call.svg", "output_phone", Placement::Icon, Paint::AsDrawn),
    ("speaker.svg", "output_speaker", Placement::Icon, Paint::AsDrawn),
    ("bluetooth.svg", "output_bluetooth", Placement::Icon, Paint::AsDrawn),
    ("headphones.svg", "output_wired", Placement::Icon, Paint::AsDrawn),
    ("speaker-off.svg", "output_mute", Placement::Icon, Paint::AsDrawn),
    ("mark.svg", "mark", Placement::Adaptive, Paint::AsDrawn),
    ("mark.svg", "mark_mono", Placement::Adaptive, Paint::White),
    ("mark.svg", "notification", Placement::Cropped, Paint::White),
];

/// Writes every target into `out` as `<name>.xml`.
pub fn write_all(icons: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    for (svg, name, placement, paint) in TARGETS {
        let source = icons.join(svg);
        let text = std::fs::read_to_string(&source).with_context(|| format!("reading {}", source.display()))?;
        let xml = convert(&text, svg, placement, paint).with_context(|| format!("converting {svg}"))?;
        let target = out.join(format!("{name}.xml"));
        std::fs::write(&target, xml).with_context(|| format!("writing {}", target.display()))?;
        println!("{} <- {svg}", target.display());
    }
    Ok(())
}

/// What a shape inherits from the elements around it, as SVG presentation attributes.
#[derive(Clone)]
struct Style {
    fill: Option<String>,
    stroke: Option<String>,
    stroke_width: Option<String>,
    linecap: Option<String>,
    linejoin: Option<String>,
}

impl Style {
    fn inherit(&self, node: roxmltree::Node) -> Self {
        let own = |name: &str, parent: &Option<String>| node.attribute(name).map(str::to_owned).or_else(|| parent.clone());
        Self {
            fill: own("fill", &self.fill),
            stroke: own("stroke", &self.stroke),
            stroke_width: own("stroke-width", &self.stroke_width),
            linecap: own("stroke-linecap", &self.linecap),
            linejoin: own("stroke-linejoin", &self.linejoin),
        }
    }
}

fn convert(text: &str, source: &str, placement: Placement, paint: Paint) -> Result<String> {
    let doc = roxmltree::Document::parse(text)?;
    let svg = doc.root_element();
    if svg.tag_name().name() != "svg" {
        bail!("the root is <{}>, not <svg>", svg.tag_name().name());
    }
    let view = view_box(svg.attribute("viewBox").context("no viewBox")?)?;
    let base = Style { fill: None, stroke: None, stroke_width: None, linecap: None, linejoin: None }.inherit(svg);

    // Size, viewport, and the group transform that places the art in it; an icon needs none.
    let (dp, viewport, group) = match placement {
        Placement::Icon => {
            if view.x != 0.0 || view.y != 0.0 {
                bail!("an icon's viewBox has to start at 0 0");
            }
            (ICON_DP, view, None)
        }
        Placement::Adaptive => {
            let scale = MARK_DP / view.width;
            let inset = (ADAPTIVE_DP - MARK_DP) / HALF;
            let layer = Rect { x: 0.0, y: 0.0, width: ADAPTIVE_DP, height: ADAPTIVE_DP };
            (ADAPTIVE_DP, layer, Some((inset, inset, scale)))
        }
        Placement::Cropped => {
            let side = MARK_ART.width.max(MARK_ART.height);
            let dx = (side - MARK_ART.width) / HALF - MARK_ART.x;
            let dy = (side - MARK_ART.height) / HALF - MARK_ART.y;
            (ICON_DP, Rect { x: 0.0, y: 0.0, width: side, height: side }, Some((dx, dy, 1.0)))
        }
    };

    let mut xml = XmlWriter::new(Options {
        use_single_quote: false,
        indent: Indent::Spaces(XML_INDENT),
        attributes_indent: Indent::Spaces(XML_INDENT),
    });
    // No `<?xml?>` declaration: optional, and this writer would spread it over four lines.
    xml.write_comment(&format!(" Generated by `just drawables` from assets/icons/{source}. Edit the SVG, not this. "));
    xml.start_element("vector");
    xml.write_attribute("xmlns:android", ANDROID_NS);
    xml.write_attribute_fmt("android:width", format_args!("{}{DP}", num(dp)));
    xml.write_attribute_fmt("android:height", format_args!("{}{DP}", num(dp)));
    xml.write_attribute("android:viewportWidth", &num(viewport.width));
    xml.write_attribute("android:viewportHeight", &num(viewport.height));
    if let Some((dx, dy, scale)) = group {
        xml.start_element("group");
        xml.write_attribute("android:translateX", &num(dx));
        xml.write_attribute("android:translateY", &num(dy));
        xml.write_attribute("android:scaleX", &num(scale));
        xml.write_attribute("android:scaleY", &num(scale));
    }
    for node in svg.children().filter(roxmltree::Node::is_element) {
        let style = base.inherit(node);
        let data = match node.tag_name().name() {
            "path" => node.attribute("d").context("a <path> without d")?.to_owned(),
            "circle" => circle(node)?,
            other => bail!("<{other}> is not something this converter draws"),
        };
        path(&mut xml, &data, &style, paint)?;
    }
    // Closes the group, if there is one, and the vector.
    let mut text = xml.end_document();
    text.push('\n');
    Ok(text)
}

fn view_box(text: &str) -> Result<Rect> {
    let numbers: Vec<f64> = text.split_whitespace().map(str::parse).collect::<Result<_, _>>()?;
    let [x, y, width, height] = numbers[..] else { bail!("viewBox {text} is not four numbers") };
    Ok(Rect { x, y, width, height })
}

/// A circle as two half-circle arcs, which is how a path draws one.
fn circle(node: roxmltree::Node) -> Result<String> {
    let get = |name: &str| -> Result<f64> {
        node.attribute(name).with_context(|| format!("a <circle> without {name}"))?.parse().map_err(Into::into)
    };
    let (cx, cy, r) = (get("cx")?, get("cy")?, get("r")?);
    Ok(format!(
        "M{} {} a{r} {r} 0 1 0 {} 0 a{r} {r} 0 1 0 {} 0 z",
        num(cx - r),
        num(cy),
        num(r * HALF),
        num(-r * HALF),
        r = num(r)
    ))
}

fn path(xml: &mut XmlWriter, data: &str, style: &Style, paint: Paint) -> Result<()> {
    let colour = |text: &str| -> Result<String> {
        Ok(match paint {
            Paint::White => WHITE.to_owned(),
            Paint::AsDrawn => argb(text)?,
        })
    };
    let mut attrs = vec![("pathData", data.to_owned())];
    let fill = style.fill.as_deref().unwrap_or(SVG_DEFAULT_FILL);
    if fill != "none" {
        attrs.push(("fillColor", colour(fill)?));
    }
    if let Some(stroke) = style.stroke.as_deref().filter(|s| *s != "none") {
        attrs.push(("strokeColor", colour(stroke)?));
        if let Some(width) = &style.stroke_width {
            attrs.push(("strokeWidth", width.clone()));
        }
        if let Some(cap) = &style.linecap {
            attrs.push(("strokeLineCap", cap.clone()));
        }
        if let Some(join) = &style.linejoin {
            attrs.push(("strokeLineJoin", join.clone()));
        }
    }
    xml.start_element("path");
    for (name, value) in attrs {
        xml.write_attribute_fmt(&format!("android:{name}"), format_args!("{value}"));
    }
    xml.end_element();
    Ok(())
}

/// `#rgb` or `#rrggbb` as Android's `#AARRGGBB`, opaque.
fn argb(text: &str) -> Result<String> {
    let body = text.strip_prefix('#').with_context(|| format!("colour {text} is not #rgb or #rrggbb"))?;
    let full: String = match body.len() {
        3 => body.chars().flat_map(|c| [c, c]).collect(),
        6 => body.to_owned(),
        _ => bail!("colour {text} is not #rgb or #rrggbb"),
    };
    Ok(format!("#FF{}", full.to_uppercase()))
}

/// A number as short as it can be written without losing what the SVG said.
fn num(value: f64) -> String {
    let text = format!("{value:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0".to_owned() } else { text.to_owned() }
}
