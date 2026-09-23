//! Dumps the rendered element tree as a table of absolute geometry and paint properties, so a
//! screen can be compared with its design by the numbers rather than by eye.
//!
//! Element names come from the Slint compiler's debug info, which it only emits when
//! `SLINT_EMIT_DEBUG_INFO` is set while this crate is compiled (`just preview` sets it).

use i_slint_core::graphics::Brush;
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::{
    BasicBorderRectangle, BorderRectangle, Clip, ClippedImage, ComplexText, ImageItem, ItemRef, Orientation, Rectangle,
    SimpleText,
};
use i_slint_core::window::WindowInner;

/// Spaces per level of nesting.
const INDENT: usize = 2;
/// Enough of a label to recognise it without wrapping the table.
const TEXT_CHARS: usize = 32;
/// Slint's "inherit the default" font weight.
const WEIGHT_DEFAULT: i32 = 0;

/// One line per drawn item: where it is in the window, how big, and what it paints.
pub fn tree(window: &slint::Window) -> String {
    let mut out = String::from("    x     y     w     h  element / paint\n");
    walk(&ItemRc::new_root(WindowInner::from_pub(window).component()), 0, &mut out);
    out
}

fn walk(item: &ItemRc, depth: usize, out: &mut String) {
    let geometry = item.geometry();
    let size = geometry.size;
    if size.width > 0. && size.height > 0. {
        // `map_to_window` accumulates the ancestors' offsets but not the item's own, so the
        // point to map is the item's origin in its parent.
        let origin = item.map_to_window(geometry.origin);
        let indent = " ".repeat(depth * INDENT);
        out.push_str(&format!(
            "{:5.0} {:5.0} {:5.0} {:5.0}  {indent}{} {}{}\n",
            origin.x,
            origin.y,
            size.width,
            size.height,
            names(item),
            paint(item),
            ink(item, size),
        ));
    }
    let mut child = item.first_child();
    while let Some(node) = child {
        walk(&node, depth + 1, out);
        child = node.next_sibling();
    }
}

/// The markup's own names for whatever collapsed into this item, e.g. `Card < Rectangle`.
fn names(item: &ItemRc) -> String {
    let Some(count) = item.element_count() else {
        return String::from("?");
    };
    (0..count)
        .filter_map(|index| item.element_type_names_and_ids(index))
        .flatten()
        .map(|(kind, id)| if id.is_empty() { kind.to_string() } else { format!("{kind}#{id}") })
        .collect::<Vec<_>>()
        .join(" < ")
}

/// What the item would take if the layout did not stretch it. For text this is the glyph run, so
/// the width a string actually occupies is comparable with the design's.
fn ink(item: &ItemRc, size: i_slint_core::lengths::LogicalSize) -> String {
    let Some(adapter) = item.window_adapter() else {
        return String::new();
    };
    let borrowed = item.borrow();
    let measure = |orientation| borrowed.as_ref().layout_info(orientation, f32::MAX, &adapter, item).preferred;
    let (width, height) = (measure(Orientation::Horizontal), measure(Orientation::Vertical));
    if width <= 0. || (width - size.width).abs() < 1. && (height - size.height).abs() < 1. {
        return String::new();
    }
    format!(" ink={width:.0}x{height:.0}")
}

fn paint(item: &ItemRc) -> String {
    let item = item.borrow();
    if let Some(rect) = ItemRef::downcast_pin::<Rectangle>(item) {
        return format!("bg={}", brush(&rect.background()));
    }
    if let Some(rect) = ItemRef::downcast_pin::<BasicBorderRectangle>(item) {
        return border(&rect.background(), rect.border_radius().get(), rect.border_width().get(), &rect.border_color());
    }
    if let Some(rect) = ItemRef::downcast_pin::<BorderRectangle>(item) {
        let radius = rect.border_radius().get().max(rect.border_top_left_radius().get());
        return border(&rect.background(), radius, rect.border_width().get(), &rect.border_color());
    }
    if let Some(clip) = ItemRef::downcast_pin::<Clip>(item) {
        return format!("clip r={:.0}", clip.border_top_left_radius().get());
    }
    if let Some(text) = ItemRef::downcast_pin::<SimpleText>(item) {
        return label(&text.text(), text.font_size().get(), text.font_weight(), &text.color(), "");
    }
    if let Some(text) = ItemRef::downcast_pin::<ComplexText>(item) {
        return label(&text.text(), text.font_size().get(), text.font_weight(), &text.color(), &text.font_family());
    }
    if let Some(image) = ItemRef::downcast_pin::<ImageItem>(item) {
        return picture(image.image_fit(), &image.colorize());
    }
    if let Some(image) = ItemRef::downcast_pin::<ClippedImage>(item) {
        return picture(image.image_fit(), &image.colorize());
    }
    String::new()
}

fn border(background: &Brush, radius: f32, width: f32, color: &Brush) -> String {
    let mut out = format!("bg={}", brush(background));
    if radius > 0. {
        out.push_str(&format!(" r={radius:.0}"));
    }
    if width > 0. {
        out.push_str(&format!(" border={width:.0} {}", brush(color)));
    }
    out
}

fn label(text: &str, size: f32, weight: i32, color: &Brush, family: &str) -> String {
    let short: String = text.chars().take(TEXT_CHARS).collect();
    let mut out = format!("{short:?} {size:.0}px");
    if weight != WEIGHT_DEFAULT {
        out.push_str(&format!(" w{weight}"));
    }
    out.push_str(&format!(" {}", brush(color)));
    if !family.is_empty() {
        out.push_str(&format!(" {family}"));
    }
    out
}

fn picture(fit: i_slint_core::items::ImageFit, colorize: &Brush) -> String {
    let mut out = format!("image fit={fit:?}");
    if colorize.color().alpha() > 0 {
        out.push_str(&format!(" colorize={}", brush(colorize)));
    }
    out
}

fn brush(value: &Brush) -> String {
    let color = value.color();
    if color.alpha() == 0 {
        return String::from("none");
    }
    if color.alpha() == u8::MAX {
        return format!("#{:02X}{:02X}{:02X}", color.red(), color.green(), color.blue());
    }
    format!("#{:02X}{:02X}{:02X}{:02X}", color.red(), color.green(), color.blue(), color.alpha())
}
