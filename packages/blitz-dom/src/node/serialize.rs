//! Serialization of nodes to HTML, and of inline `<svg>` subtrees to SVG source for usvg.

use html_escape::{encode_quoted_attribute_to_string, encode_text_to_string};
use markup5ever::{QualName, ns};
use style_traits::values::ToCss;

use super::{Node, NodeData};

#[derive(Clone, Copy)]
enum OutputStyle {
    Normal,
    Pretty,
}

#[derive(Clone, Copy)]
struct SerializeOpts {
    style: OutputStyle,
    /// Replace `currentColor` in attribute values with the element's computed `color`
    resolve_current_color: bool,
    /// Declare the `xlink` prefix on the root element so that the output is well-formed XML
    declare_xlink_ns: bool,
}

impl SerializeOpts {
    const fn html(style: OutputStyle) -> Self {
        Self {
            style,
            resolve_current_color: false,
            declare_xlink_ns: false,
        }
    }
}

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Writes an attribute's serialized name, per the HTML fragment serialization algorithm
fn write_attr_name(name: &QualName, writer: &mut String) {
    match name.ns {
        ns!() | ns!(html) => {}
        ns!(xml) => writer.push_str("xml:"),
        ns!(xmlns) if name.local.as_str() == "xmlns" => {}
        ns!(xmlns) => writer.push_str("xmlns:"),
        ns!(xlink) => writer.push_str("xlink:"),
        _ => {
            if let Some(prefix) = &name.prefix {
                writer.push_str(prefix);
                writer.push(':');
            }
        }
    }
    writer.push_str(&name.local);
}

fn is_xlink_ns_decl(name: &QualName) -> bool {
    (name.ns == ns!(xmlns) && name.local.as_str() == "xlink")
        || name.local.as_str() == "xmlns:xlink"
}

impl Node {
    /// Renders the HTML of this node and all its children as a `String` without extra whitespace.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html><head /><body><main id="main"><div class="arbitrary-class" /></main></body></html>
    /// ```
    pub fn outer_html(&self) -> String {
        let mut output = String::new();
        self.write_outer_html(&mut output);
        output
    }

    /// Renders the HTML of this node and all its children as a `String` with whitespace for human
    /// readability.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html>
    ///   <head />
    ///   <body>
    ///     <main id="main">
    ///       <div class="arbitrary-class" />
    ///     </main>
    ///   </body>
    /// </html>
    /// ```
    pub fn outer_html_pretty(&self) -> String {
        let mut output = String::new();
        self.write_outer_html_pretty(&mut output);
        output
    }

    pub fn write_outer_html(&self, writer: &mut String) {
        self.write_outer_html_in_style(writer, SerializeOpts::html(OutputStyle::Normal), 0);
    }

    pub fn write_outer_html_pretty(&self, writer: &mut String) {
        self.write_outer_html_in_style(writer, SerializeOpts::html(OutputStyle::Pretty), 0);
    }

    /// Serializes this `<svg>` subtree as a standalone SVG document for usvg, with
    /// `currentColor` resolved against each element's computed `color`.
    ///
    /// acrender: page CSS for the SVG's children (`.icon .stroke { fill: none; stroke: currentColor }`)
    /// and inline `style="font: 700 30px var(--mono)"` used to be lost (usvg sees neither the page's
    /// stylesheets nor `var()`): icons came out as black blocks and `<text>` disappeared. Each
    /// element's computed SVG paint and font properties are now written into the serialized `style`
    /// (replacing the authored one, which Stylo has already folded in), and `display: none` children
    /// are left out. Stylo gets no SVG presentation attributes (`fill="none"`) from Blitz, so a
    /// property equal to the parent's is inherited (or comes from an attribute) and is not written;
    /// the root writes every property it has no attribute for.
    #[cfg(feature = "svg")]
    pub(crate) fn svg_source(&self) -> String {
        let mut output = String::new();
        self.write_svg(&mut output, true, self.svg_viewport());
        output
    }

    /// user units of a root `<svg>`: its viewBox, else its laid-out size (for percentages in CSS geometry)
    #[cfg(feature = "svg")]
    pub(crate) fn svg_viewport(&self) -> (f32, f32) {
        self.element_data()
            .and_then(|e| {
                e.attrs()
                    .iter()
                    .find(|a| &*a.name.local == "viewBox")
                    .map(|a| a.value.clone())
            })
            .and_then(|v| {
                let n: Vec<f32> = v
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse().ok())
                    .collect();
                (n.len() == 4).then(|| (n[2], n[3]))
            })
            .unwrap_or((
                self.final_layout().size.width,
                self.final_layout().size.height,
            ))
    }

    /// acrender: SVG2 geometry given in CSS (`.dash rect { width: calc(100% - 3px) }`) is invisible to usvg,
    /// which only reads attributes: the computed values, resolved against the SVG viewport, in user units.
    /// Only properties CSS actually set (not their initial values) are returned.
    #[cfg(feature = "svg")]
    fn svg_geometry(
        &self,
        tag: &str,
        style: &style::properties::ComputedValues,
        viewport: (f32, f32),
    ) -> Vec<(&'static str, f32)> {
        use style::Zero;
        use style::values::computed::Length;
        use style::values::generics::length::{GenericLengthPercentageOrAuto, GenericSize};
        let (w, h) = viewport;
        let diagonal = ((w * w + h * h) / 2.0).sqrt();
        let lp = |v: &style::values::computed::LengthPercentage, basis: f32| {
            v.resolve(Length::new(basis)).px()
        };
        let svg = style.get_svg();
        let mut out = Vec::new();
        let mut put = |name: &'static str, value: f32, initial: bool| {
            if !initial {
                out.push((name, value));
            }
        };
        let auto = |v: &GenericLengthPercentageOrAuto<
            style::values::generics::NonNegative<style::values::computed::LengthPercentage>,
        >,
                    basis: f32| match v {
            GenericLengthPercentageOrAuto::LengthPercentage(v) => Some(lp(&v.0, basis)),
            GenericLengthPercentageOrAuto::Auto => None,
        };
        let size = |v: &GenericSize<
            style::values::generics::NonNegative<style::values::computed::LengthPercentage>,
        >,
                    basis: f32| match v {
            GenericSize::LengthPercentage(v) => Some(lp(&v.0, basis)),
            _ => None,
        };
        match tag {
            "circle" | "ellipse" => {
                put("cx", lp(&svg.cx, w), svg.cx.is_zero());
                put("cy", lp(&svg.cy, h), svg.cy.is_zero());
                if tag == "circle" {
                    put("r", lp(&svg.r.0, diagonal), svg.r.0.is_zero());
                }
            }
            "rect" | "image" | "foreignObject" | "use" => {
                put("x", lp(&svg.x, w), svg.x.is_zero());
                put("y", lp(&svg.y, h), svg.y.is_zero());
                let position = style.get_position();
                if let Some(v) = size(&position.width, w) {
                    put("width", v, false);
                }
                if let Some(v) = size(&position.height, h) {
                    put("height", v, false);
                }
            }
            _ => {}
        }
        if matches!(tag, "rect" | "ellipse") {
            if let Some(v) = auto(&svg.rx, w) {
                put("rx", v, false);
            }
            if let Some(v) = auto(&svg.ry, h) {
                put("ry", v, false);
            }
        }
        out
    }

    /// acrender: one SVG element as `svg_source` writes it: its attributes (currentColor resolved), the geometry CSS
    /// gives it, and the `style` declarations (computed values not inherited unchanged from the parent).
    /// `None` when it is left out (no style, display: none, a shape with zero size). The simple-SVG builder
    /// (simple_svg.rs) reads the same pairs straight from here instead of parsing the text.
    #[cfg(feature = "svg")]
    pub(crate) fn svg_element(&self, root: bool, viewport: (f32, f32)) -> Option<SvgElement<'_>> {
        use style::properties::ComputedValues;
        use style_traits::ToCss;
        // acrender: (name, the computed value as CSS, whether two styles have the same computed value): the
        // values are compared as values and only the ones written get formatted (animated icons rebuild
        // every frame; formatting all of them twice was most of the build)
        type Prop = (
            &'static str,
            fn(&ComputedValues) -> String,
            fn(&ComputedValues, &ComputedValues) -> bool,
        );
        macro_rules! props {
            ($($name:literal => $clone:ident),* $(,)?) => { [$((
                $name,
                (|s: &ComputedValues| s.$clone().to_css_string()) as fn(&ComputedValues) -> String,
                (|a: &ComputedValues, b: &ComputedValues| a.$clone() == b.$clone()) as fn(&ComputedValues, &ComputedValues) -> bool,
            )),*] };
        }
        const COUNT: usize = 18;
        let mut props: [Prop; COUNT] = props! {
            "fill" => clone_fill, "fill-opacity" => clone_fill_opacity, "fill-rule" => clone_fill_rule,
            "stroke" => clone_stroke, "stroke-width" => clone_stroke_width, "stroke-opacity" => clone_stroke_opacity,
            "stroke-linecap" => clone_stroke_linecap, "stroke-linejoin" => clone_stroke_linejoin,
            "stroke-miterlimit" => clone_stroke_miterlimit, "stroke-dasharray" => clone_stroke_dasharray,
            "stroke-dashoffset" => clone_stroke_dashoffset, "opacity" => clone_opacity, "visibility" => clone_visibility,
            "color" => clone_color, "font-family" => clone_font_family, "font-size" => clone_font_size,
            "font-weight" => clone_font_weight, "font-style" => clone_font_style,
        };
        // acrender: colors as sRGB rgb() / rgba(): usvg reads no oklch(), lab(), color-mix() ... (Tailwind 4's palette)
        for prop in props.iter_mut() {
            prop.1 = match prop.0 {
                "color" => |s: &ComputedValues| srgb_css(&s.clone_color()),
                "fill" => |s: &ComputedValues| svg_paint_css(&s.clone_fill(), s),
                "stroke" => |s: &ComputedValues| svg_paint_css(&s.clone_stroke(), s),
                _ => continue,
            };
        }
        let NodeData::Element(e) = &self.data else {
            return None;
        };
        let style = self.primary_styles()?;
        if style.get_box().display.is_none() {
            return None;
        }
        let parent = self.parent.and_then(|p| self.tree()[p].primary_styles());
        // usvg only knows `currentColor`; Stylo writes `currentcolor` (formatted only when a value uses it)
        let color = std::cell::OnceCell::new();
        let resolve = |v: &str| match v.contains("urrent")
            && (v.contains("currentColor") || v.contains("currentcolor"))
        {
            true => {
                let color = color.get_or_init(|| srgb_css(&style.clone_color()));
                v.replace("currentColor", color)
                    .replace("currentcolor", color)
            }
            false => v.to_owned(),
        };
        let tag = &*e.name.local;
        let geometry: Vec<(&str, f32)> = self
            .svg_geometry(tag, &style, viewport)
            .into_iter()
            .filter(|(name, _)| !e.attrs().iter().any(|a| &*a.name.local == *name))
            .collect();
        // a shape with zero size draws nothing: leave it out instead of letting usvg warn about it
        // (a hidden highlight circle with no r yet, an empty bar)
        let number = |name: &str| {
            geometry
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| Some(*v))
                .unwrap_or_else(|| {
                    let a = e.attrs().iter().find(|a| &*a.name.local == name)?;
                    a.value
                        .trim()
                        .trim_end_matches("px")
                        .parse::<f32>()
                        .ok()
                        .or(Some(f32::NAN))
                })
        };
        let empty = |name: &str| number(name).is_none_or(|v| v == 0.0);
        if (tag == "circle" && empty("r")) || (tag == "rect" && (empty("width") || empty("height")))
        {
            return None;
        }
        // acrender: a CSS transform (a class, an animation) wins over the `transform` attribute, as in
        // browsers; usvg only reads the attribute, so the computed one is written in its place. The root's
        // transform is its box's and is painted with the box
        let css_transform = (!root)
            .then(|| {
                let (w, h) = viewport;
                let reference_box = euclid::Rect::new(
                    euclid::Point2D::new(
                        style::values::computed::CSSPixelLength::new(0.0),
                        style::values::computed::CSSPixelLength::new(0.0),
                    ),
                    euclid::Size2D::new(
                        style::values::computed::CSSPixelLength::new(w),
                        style::values::computed::CSSPixelLength::new(h),
                    ),
                );
                crate::resolve_2d_transform(style.get_box(), reference_box)
            })
            .flatten();
        let mut attrs = Vec::new();
        for attr in e.attrs().iter().filter(|a| {
            &*a.name.local != "style"
                && &*a.name.local != "xmlns"
                && !(css_transform.is_some() && &*a.name.local == "transform")
        }) {
            let mut name = String::new();
            write_attr_name(&attr.name, &mut name);
            attrs.push((name, resolve(&attr.value)));
        }
        for (name, value) in &geometry {
            attrs.push(((*name).to_owned(), format!("{value}")));
        }
        if let Some(t) = css_transform {
            let [a, b, c, d, e, f] = t.as_coeffs();
            attrs.push((
                "transform".to_owned(),
                format!("matrix({a} {b} {c} {d} {e} {f})"),
            ));
        }
        let has_attr = |name: &str| e.attrs().iter().any(|a| &*a.name.local == name);
        let mut css = Vec::new();
        for (name, value, same) in props {
            let inherited = parent.as_ref().is_some_and(|p| same(p, &style));
            if !inherited || (root && !has_attr(name)) {
                css.push((name, resolve(&value(&style))));
            }
        }
        Some(SvgElement {
            tag,
            xlink_declared: e.attrs().iter().any(|a| is_xlink_ns_decl(&a.name)),
            attrs,
            css,
        })
    }

    #[cfg(feature = "svg")]
    pub(crate) fn write_svg(&self, out: &mut String, root: bool, viewport: (f32, f32)) {
        match &self.data {
            NodeData::Text(t) => {
                encode_text_to_string(&t.content, out);
            }
            NodeData::Element(_) => {
                let Some(el) = self.svg_element(root, viewport) else {
                    return;
                };
                out.push('<');
                out.push_str(el.tag);
                if root {
                    out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                    if !el.xlink_declared {
                        out.push_str(" xmlns:xlink=\"");
                        out.push_str(XLINK_NS);
                        out.push('"');
                    }
                }
                for (name, value) in &el.attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    encode_quoted_attribute_to_string(value, out);
                    out.push('"');
                }
                let css: String = el
                    .css
                    .iter()
                    .map(|(name, value)| format!("{name}:{value};"))
                    .collect();
                if !css.is_empty() {
                    out.push_str(" style=\"");
                    encode_quoted_attribute_to_string(&css, out);
                    out.push('"');
                }
                out.push('>');
                for &child in &self.children {
                    self.tree()[child].write_svg(out, false, viewport);
                }
                out.push_str("</");
                out.push_str(el.tag);
                out.push('>');
            }
            _ => {}
        }
    }

    fn write_outer_html_in_style(&self, writer: &mut String, opts: SerializeOpts, nesting: usize) {
        const INDENT: &str = "  ";
        let style = opts.style;
        let has_children = !self.children.is_empty();
        let current_color = opts
            .resolve_current_color
            .then(|| self.primary_styles())
            .flatten()
            .map(|style| srgb_css(&style.clone_color()));

        match &self.data {
            NodeData::Document(_) => {}
            NodeData::Comment { .. } => {}
            NodeData::AnonymousBlock(_) => {}
            // NodeData::Doctype { name, .. } => write!(s, "DOCTYPE {name}"),
            NodeData::Text(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                let in_raw_text_element = self
                    .parent
                    .and_then(|id| self.tree()[id].data.downcast_element())
                    .is_some_and(|el| {
                        // Documents are parsed with scripting disabled, so `<noscript>` is not raw text
                        el.name.ns == ns!(html)
                            && matches!(
                                el.name.local.as_ref(),
                                "style"
                                    | "script"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "plaintext"
                            )
                    });
                if in_raw_text_element {
                    writer.push_str(data.content.as_str());
                } else {
                    encode_text_to_string(data.content.as_str(), writer);
                }
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }
            }
            NodeData::Element(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                writer.push('<');
                writer.push_str(&data.name.local);

                if opts.declare_xlink_ns
                    && nesting == 0
                    && !data.attrs().iter().any(|attr| is_xlink_ns_decl(&attr.name))
                {
                    writer.push_str(" xmlns:xlink=\"");
                    writer.push_str(XLINK_NS);
                    writer.push('"');
                }

                for attr in data.attrs() {
                    writer.push(' ');
                    write_attr_name(&attr.name, writer);
                    writer.push_str("=\"");
                    #[allow(clippy::unnecessary_unwrap)] // Convert to if-let chain once stabilised
                    if current_color.is_some() && attr.value.contains("currentColor") {
                        let value = attr
                            .value
                            .replace("currentColor", current_color.as_ref().unwrap());
                        encode_quoted_attribute_to_string(&value, writer);
                    } else {
                        encode_quoted_attribute_to_string(&attr.value, writer);
                    }
                    writer.push('"');
                }
                if !has_children {
                    writer.push_str(" /");
                }
                writer.push('>');
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }

                if has_children {
                    for &child_id in &self.children {
                        self.tree()[child_id].write_outer_html_in_style(writer, opts, nesting + 1);
                    }

                    if matches!(style, OutputStyle::Pretty) {
                        for _ in 0..nesting {
                            writer.push_str(INDENT);
                        }
                    }
                    writer.push_str("</");
                    writer.push_str(&data.name.local);
                    writer.push('>');
                    if matches!(style, OutputStyle::Pretty) {
                        writer.push('\n');
                    }
                }
            }
        }
    }
}

/// acrender: see [`Node::svg_element`]
#[cfg(feature = "svg")]
pub(crate) struct SvgElement<'a> {
    pub(crate) tag: &'a str,
    /// the element itself declares the xlink namespace
    pub(crate) xlink_declared: bool,
    /// attributes as written (namespaced ones with their prefix), then geometry from CSS
    pub(crate) attrs: Vec<(String, String)>,
    /// the `style` declarations
    pub(crate) css: Vec<(&'static str, String)>,
}

/// acrender: a color as `rgb()` / `rgba()` in sRGB, components clipped to its gamut
pub(crate) fn srgb_css(color: &style::color::AbsoluteColor) -> String {
    let c = color.to_color_space(style::color::ColorSpace::Srgb);
    let channel = |v: f32| if v.is_nan() { 0 } else { (v.clamp(0.0, 1.0) * 255.0).round() as u8 };
    let (r, g, b) = (channel(c.components.0), channel(c.components.1), channel(c.components.2));
    let alpha = if c.alpha.is_nan() { 0.0 } else { c.alpha.clamp(0.0, 1.0) };
    match alpha >= 1.0 {
        true => format!("rgb({r},{g},{b})"),
        false => format!("rgba({r},{g},{b},{alpha})"),
    }
}

/// acrender: an SVG paint with its color (currentColor and color-mix() resolved) written as sRGB
fn svg_paint_css(paint: &style::values::computed::SVGPaint, style: &style::properties::ComputedValues) -> String {
    use style::values::generics::svg::{SVGPaintFallback, SVGPaintKind};
    let current = style.clone_color();
    match (&paint.kind, &paint.fallback) {
        (SVGPaintKind::Color(color), _) => srgb_css(&color.resolve_to_absolute(&current)),
        (SVGPaintKind::PaintServer(url), SVGPaintFallback::Color(color)) => {
            format!("{} {}", url.to_css_string(), srgb_css(&color.resolve_to_absolute(&current)))
        }
        _ => paint.to_css_string(),
    }
}
