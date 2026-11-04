//! acrender: a fast path for simple inline SVGs, bypassing usvg.
//!
//! An inline `<svg>` is rebuilt (serialized and parsed) on every style change in its subtree; for
//! icons whose `<path d>` is animated every frame (os-v2's Dopa faces) the usvg parse dominated
//! style resolution. SVGs made only of `path/circle/ellipse/rect/line/polyline/polygon/g` with
//! solid fills and strokes are turned straight into kurbo paths here and painted by blitz-paint.
//! Anything else (gradients, text, use, clip paths, masks, filters, dashes, percentages, a
//! non-default `preserveAspectRatio` ...) returns `None` and goes through usvg as before.
//!
//! Input is the source produced by `Node::svg_source`, where computed CSS has already been written
//! into each element's `style` with concrete values (`rgb(...)`, `none`, lengths in px).
use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;

use kurbo::{Affine, BezPath, Cap, Circle, Ellipse, Join, Line, Rect, RoundedRect, Shape, Stroke};
use usvg::roxmltree;

/// Straight (not premultiplied) RGBA in 0..=1
pub type Rgba = [f32; 4];

#[derive(Debug, Clone)]
pub enum SimpleOp {
    Fill {
        path: BezPath,
        transform: Affine,
        color: Rgba,
        even_odd: bool,
    },
    Stroke {
        path: BezPath,
        transform: Affine,
        stroke: Stroke,
        color: Rgba,
    },
    /// A `<text>` (with its inherited paint and font), parsed by usvg on its own and cached by source:
    /// the text rarely changes even when the paths around it animate every frame
    Tree {
        tree: Arc<usvg::Tree>,
        transform: Affine,
    },
    /// Group opacity (an element with opacity whose content overlaps itself); `bounds` covers what is
    /// drawn up to the matching `Pop`, in the same space as the ops' transforms
    PushOpacity {
        alpha: f32,
        bounds: Rect,
    },
    Pop,
}

/// Drawing commands in the SVG viewport (`size`) coordinate space, `view_box` already applied
#[derive(Debug, Clone)]
pub struct SimpleSvg {
    pub ops: Vec<SimpleOp>,
}

const ELEMENTS: &[&str] = &[
    "svg", "g", "path", "circle", "ellipse", "rect", "line", "polyline", "polygon", "title", "desc",
];
const ATTRIBUTES: &[&str] = &[
    "id",
    "class",
    "style",
    "d",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "x",
    "y",
    "width",
    "height",
    "x1",
    "y1",
    "x2",
    "y2",
    "points",
    "transform",
    "fill",
    "stroke",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "opacity",
    "fill-opacity",
    "stroke-opacity",
    "fill-rule",
    "viewBox",
    "preserveAspectRatio",
    "version",
    "role",
    "focusable",
    "xmlns",
    "color",
    "visibility",
    "stroke-dasharray",
    "stroke-dashoffset",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "pathLength",
];

/// Inherited paint state
#[derive(Clone, Copy)]
struct Paint {
    fill: Option<Rgba>,
    fill_opacity: f32,
    even_odd: bool,
    stroke: Option<Rgba>,
    stroke_width: f64,
    stroke_opacity: f32,
    cap: Cap,
    join: Join,
    miter: f64,
    visible: bool,
}

impl Default for Paint {
    fn default() -> Self {
        Paint {
            fill: Some([0.0, 0.0, 0.0, 1.0]),
            fill_opacity: 1.0,
            even_odd: false,
            stroke: None,
            stroke_width: 1.0,
            stroke_opacity: 1.0,
            cap: Cap::Butt,
            join: Join::Miter,
            miter: 4.0,
            visible: true,
        }
    }
}

impl SimpleOp {
    /// What `ops` can paint (a bit more for strokes' joins and antialiasing)
    fn bounds(ops: &[SimpleOp]) -> Rect {
        ops.iter()
            .filter_map(|op| match op {
                SimpleOp::Fill {
                    path, transform, ..
                } => Some(transform.transform_rect_bbox(path.bounding_box())),
                SimpleOp::Stroke {
                    path,
                    transform,
                    stroke,
                    ..
                } => {
                    let pad = stroke.width / 2.0 * stroke.miter_limit.max(1.0);
                    Some(transform.transform_rect_bbox(path.bounding_box().inflate(pad, pad)))
                }
                SimpleOp::Tree { tree, transform } => {
                    let r = tree.root().abs_layer_bounding_box();
                    let r = Rect::new(
                        r.left().into(),
                        r.top().into(),
                        r.right().into(),
                        r.bottom().into(),
                    );
                    Some(transform.transform_rect_bbox(r))
                }
                SimpleOp::PushOpacity { bounds, .. } => Some(*bounds),
                SimpleOp::Pop => None,
            })
            .reduce(|a, b| a.union(b))
            .unwrap_or(Rect::ZERO)
            .inflate(1.0, 1.0)
    }
}

impl SimpleSvg {
    /// `size` is the SVG viewport size (what usvg reports as `Tree::size`)
    /// `text` parses a standalone SVG holding one `<text>` (cached by the caller)
    pub fn parse(
        source: &str,
        size: (f64, f64),
        text: &dyn Fn(&str) -> Option<Arc<usvg::Tree>>,
    ) -> Option<SimpleSvg> {
        let doc = roxmltree::Document::parse(source).ok()?;
        Self::build(El::Xml(doc.root_element()), source, size, text)
    }

    /// acrender: the same, straight from the DOM: the pairs `svg_source` would write, without writing and
    /// parsing the text (animated icons rebuild every frame). `None` wherever the text path could differ
    pub(crate) fn from_dom(
        node: &crate::Node,
        viewport: (f32, f32),
        size: (f64, f64),
        text: &dyn Fn(&str) -> Option<Arc<usvg::Tree>>,
    ) -> Option<SimpleSvg> {
        Self::build(El::Dom(node, viewport), "", size, text)
    }

    fn build(
        root: El,
        source: &str,
        size: (f64, f64),
        text: &dyn Fn(&str) -> Option<Arc<usvg::Tree>>,
    ) -> Option<SimpleSvg> {
        let par = root.attr("preserveAspectRatio");
        let par = par.as_deref().map(str::trim);
        if par.is_some_and(|p| p != "xMidYMid meet" && p != "xMidYMid") {
            return None;
        }
        let view_box = match root.attr("viewBox").as_deref() {
            Some(v) => {
                let n: Vec<f64> = v
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().ok())
                    .collect::<Option<_>>()?;
                let [x, y, w, h] = n[..] else { return None };
                if w <= 0.0 || h <= 0.0 {
                    return None;
                }
                // xMidYMid meet
                let k = (size.0 / w).min(size.1 / h);
                Affine::translate(((size.0 - w * k) / 2.0, (size.1 - h * k) / 2.0))
                    * Affine::scale(k)
                    * Affine::translate((-x, -y))
            }
            None => Affine::IDENTITY,
        };
        let mut svg = SimpleSvg { ops: Vec::new() };
        let mut cx = Cx {
            source,
            text,
            fonts: Vec::new(),
        };
        svg.element(&mut cx, root, Paint::default(), view_box, true)?;
        Some(svg)
    }

    fn element(
        &mut self,
        cx: &mut Cx,
        el: El,
        inherited: Paint,
        parent: Affine,
        root: bool,
    ) -> Option<()> {
        let name = el.name();
        if name == "text" {
            return self.text(cx, el, inherited, parent);
        }
        if !ELEMENTS.contains(&name) {
            return None;
        }
        if matches!(name, "title" | "desc") {
            return Some(());
        }
        // a nested <svg> is a new viewport (x/y/width/height/viewBox, clipping): left to usvg
        if name == "svg" && !root {
            return None;
        }
        // left out (svg_source doesn't write it): nothing to draw
        let Some(style) = el.style(root)? else {
            return Some(());
        };
        let mut paint = inherited;
        style.paint(&mut paint)?;
        let transform = match (root, style.get("transform")) {
            (false, Some(t)) => parent * Self::transform(t)?,
            _ => parent,
        };
        let opacity = style.number("opacity")?.unwrap_or(1.0) as f32;
        if opacity <= 0.0 {
            return Some(());
        }
        let shape = Self::shape(name, &style)?;
        // Opacity on a single shape with only a fill or only a stroke folds into the paint;
        // otherwise (groups, fill + stroke overlapping) it needs a layer
        let single = shape.is_some() && !(paint.fill.is_some() && paint.stroke.is_some());
        let layer = opacity < 1.0 && !single;
        let at = self.ops.len();
        if layer {
            self.ops.push(SimpleOp::PushOpacity {
                alpha: opacity,
                bounds: Rect::ZERO,
            });
        }
        let alpha = if layer { 1.0 } else { opacity };
        match shape {
            Some(path) if paint.visible => {
                if let Some(mut c) = paint.fill {
                    c[3] *= paint.fill_opacity * alpha;
                    self.ops.push(SimpleOp::Fill {
                        path: path.clone(),
                        transform,
                        color: c,
                        even_odd: paint.even_odd,
                    });
                }
                if let Some(mut c) = paint.stroke.filter(|_| paint.stroke_width > 0.0) {
                    c[3] *= paint.stroke_opacity * alpha;
                    let stroke = Stroke::new(paint.stroke_width)
                        .with_caps(paint.cap)
                        .with_join(paint.join)
                        .with_miter_limit(paint.miter);
                    self.ops.push(SimpleOp::Stroke {
                        path,
                        transform,
                        stroke,
                        color: c,
                    });
                }
            }
            Some(_) => {}
            None => {
                let fonts = cx.fonts.len();
                cx.fonts.extend(
                    FONT.iter()
                        .filter_map(|&f| Some((f, style.get(f)?.to_owned()))),
                );
                for child in el.children()? {
                    self.element(cx, child, paint, transform, false)?;
                }
                cx.fonts.truncate(fonts);
            }
        }
        if layer {
            let content = SimpleOp::bounds(&self.ops[at + 1..]);
            if let SimpleOp::PushOpacity { bounds, .. } = &mut self.ops[at] {
                *bounds = content;
            }
            self.ops.push(SimpleOp::Pop);
        }
        Some(())
    }

    /// A `<text>`: its own source (it keeps its own transform/opacity, usvg applies them) wrapped in a
    /// `<g>` carrying the inherited paint and font, parsed through the cache
    fn text(&mut self, cx: &mut Cx, el: El, paint: Paint, parent: Affine) -> Option<()> {
        let raw = el.raw(cx.source)?;
        // references (gradients, paths) point outside this element
        if raw.contains("url(") || raw.contains("href") {
            return None;
        }
        let color = |c: Option<Rgba>| match c {
            Some([r, g, b, a]) => format!(
                "rgba({},{},{},{a})",
                (r * 255.0).round(),
                (g * 255.0).round(),
                (b * 255.0).round()
            ),
            None => "none".into(),
        };
        let mut style = format!(
            "fill:{};fill-opacity:{};stroke:{};stroke-width:{};stroke-opacity:{};",
            color(paint.fill),
            paint.fill_opacity,
            color(paint.stroke),
            paint.stroke_width,
            paint.stroke_opacity
        );
        for (name, value) in &cx.fonts {
            style.push_str(&format!("{name}:{value};"));
        }
        let style = style
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;");
        let mini = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><g style=\"{style}\">{raw}</g></svg>"
        );
        let tree = (cx.text)(&mini)?;
        if paint.visible {
            self.ops.push(SimpleOp::Tree {
                tree,
                transform: parent,
            });
        }
        Some(())
    }

    /// The element's geometry; `Some(None)` for containers
    fn shape(name: &str, style: &Style) -> Option<Option<BezPath>> {
        let len = |n: &str| style.length(n).map(|v| v.unwrap_or(0.0));
        Some(Some(match name {
            "svg" | "g" => return Some(None),
            "path" => BezPath::from_svg(style.get("d").unwrap_or("")).ok()?,
            "circle" => Circle::new((len("cx")?, len("cy")?), len("r")?).to_path(0.1),
            "ellipse" => {
                Ellipse::new((len("cx")?, len("cy")?), (len("rx")?, len("ry")?), 0.0).to_path(0.1)
            }
            "rect" => {
                let (x, y, w, h) = (len("x")?, len("y")?, len("width")?, len("height")?);
                let (rx, ry) = (style.length("rx")?, style.length("ry")?);
                let r = rx.or(ry).unwrap_or(0.0).min(w / 2.0).min(h / 2.0);
                if rx.is_some() && ry.is_some() && rx != ry {
                    return None;
                }
                RoundedRect::from_rect(Rect::new(x, y, x + w, y + h), r).to_path(0.1)
            }
            "line" => Line::new((len("x1")?, len("y1")?), (len("x2")?, len("y2")?)).to_path(0.1),
            "polyline" | "polygon" => {
                let n: Vec<f64> = style
                    .get("points")
                    .unwrap_or("")
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().ok())
                    .collect::<Option<_>>()?;
                let mut path = BezPath::new();
                for (i, p) in n.as_chunks::<2>().0.iter().enumerate() {
                    match i {
                        0 => path.move_to((p[0], p[1])),
                        _ => path.line_to((p[0], p[1])),
                    }
                }
                if name == "polygon" {
                    path.close_path();
                }
                path
            }
            _ => return None,
        }))
    }

    fn transform(value: &str) -> Option<Affine> {
        let t = svgtypes::Transform::from_str(value).ok()?;
        Some(Affine::new([t.a, t.b, t.c, t.d, t.e, t.f]))
    }
}

/// Font properties inherited down to `<text>`
const FONT: &[&str] = &["font-family", "font-size", "font-weight", "font-style"];

/// Per-parse context
struct Cx<'a> {
    source: &'a str,
    text: &'a dyn Fn(&str) -> Option<Arc<usvg::Tree>>,
    /// font properties set on ancestors (outermost first; later ones win in the mini SVG's style)
    fonts: Vec<(&'static str, String)>,
}

/// An SVG element: from the serialized text, or (acrender) straight from the DOM (see `SimpleSvg::from_dom`)
#[derive(Clone, Copy)]
enum El<'a> {
    Xml(roxmltree::Node<'a, 'a>),
    Dom(&'a crate::Node, (f32, f32)),
}

impl<'a> El<'a> {
    fn name(&self) -> &'a str {
        match self {
            El::Xml(el) => el.tag_name().name(),
            El::Dom(node, _) => node.element_data().map_or("", |e| &e.name.local),
        }
    }

    /// A root attribute as written
    fn attr(&self, name: &str) -> Option<Cow<'a, str>> {
        match self {
            El::Xml(el) => el.attribute(name).map(Cow::Borrowed),
            El::Dom(node, _) => node
                .attr(markup5ever::LocalName::from(name))
                .map(Cow::Borrowed),
        }
    }

    /// `Some(None)`: not written by `svg_source` (display: none, an empty shape); `None`: not simple
    fn style(&self, root: bool) -> Option<Option<Style<'a>>> {
        match self {
            El::Xml(el) => Style::new(*el).map(Some),
            El::Dom(node, viewport) => {
                let Some(el) = node.svg_element(root, *viewport) else {
                    return Some(None);
                };
                let mut pairs = Vec::new();
                for (name, value) in el.attrs {
                    if name.starts_with("xmlns") {
                        continue;
                    }
                    // namespaced attributes: the text path skips some; leave the decision to it
                    if name.contains(':')
                        || !(ATTRIBUTES.contains(&name.as_str())
                            || name.starts_with("aria-")
                            || name.starts_with("data-"))
                    {
                        return None;
                    }
                    pairs.push((Cow::Owned(name), Cow::Owned(value)));
                }
                for (name, value) in el.css {
                    if !ATTRIBUTES.contains(&name) {
                        return None;
                    }
                    pairs.push((Cow::Borrowed(name), Cow::Owned(value.trim().to_owned())));
                }
                Some(Some(Style { pairs }))
            }
        }
    }

    /// Child elements; `None` when there is text that isn't just whitespace
    fn children(&self) -> Option<Vec<El<'a>>> {
        let mut out = Vec::new();
        match self {
            El::Xml(el) => {
                for child in el.children() {
                    if child.is_element() {
                        out.push(El::Xml(child));
                    } else if child.is_text() && !child.text().is_some_and(|t| t.trim().is_empty())
                    {
                        return None;
                    }
                }
            }
            El::Dom(node, viewport) => {
                for &child in &node.children {
                    let child = &node.tree()[child];
                    match &child.data {
                        crate::NodeData::Element(_) => out.push(El::Dom(child, *viewport)),
                        crate::NodeData::Text(t) if !t.content.trim().is_empty() => return None,
                        _ => {}
                    }
                }
            }
        }
        Some(out)
    }

    /// A `<text>` element's own source
    fn raw(&self, source: &'a str) -> Option<Cow<'a, str>> {
        match self {
            El::Xml(el) => Some(Cow::Borrowed(&source[el.range()])),
            El::Dom(node, viewport) => {
                let mut out = String::new();
                node.write_svg(&mut out, false, *viewport);
                Some(Cow::Owned(out))
            }
        }
    }
}

/// An element's attributes overlaid with its `style` declarations (style wins, as written by `svg_source`)
struct Style<'a> {
    pairs: Vec<(Cow<'a, str>, Cow<'a, str>)>,
}

impl<'a> Style<'a> {
    fn new(el: roxmltree::Node<'a, 'a>) -> Option<Self> {
        let mut pairs = Vec::new();
        for a in el.attributes() {
            if a.namespace()
                .is_some_and(|ns| ns != "http://www.w3.org/2000/svg")
            {
                continue;
            }
            let name = a.name();
            if !(ATTRIBUTES.contains(&name)
                || name.starts_with("aria-")
                || name.starts_with("data-"))
            {
                return None;
            }
            if name != "style" {
                pairs.push((Cow::Borrowed(name), Cow::Borrowed(a.value())));
            }
        }
        for decl in el.attribute("style").unwrap_or("").split(';') {
            let Some((k, v)) = decl.split_once(':') else {
                continue;
            };
            let k = k.trim();
            if !ATTRIBUTES.contains(&k) {
                return None;
            }
            pairs.push((Cow::Borrowed(k), Cow::Borrowed(v.trim())));
        }
        Some(Style { pairs })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_ref())
    }

    /// A plain number or px length; `Err`-like `None` for anything else (percentages, em ...)
    fn length(&self, name: &str) -> Option<Option<f64>> {
        match self.get(name) {
            None => Some(None),
            Some(v) => v.trim_end_matches("px").trim().parse().ok().map(Some),
        }
    }

    fn number(&self, name: &str) -> Option<Option<f64>> {
        self.length(name)
    }

    fn paint(&self, paint: &mut Paint) -> Option<()> {
        if let Some(v) = self.get("fill") {
            paint.fill = Self::color(v)?;
        }
        if let Some(v) = self.get("stroke") {
            paint.stroke = Self::color(v)?;
        }
        if let Some(v) = self.number("fill-opacity")? {
            paint.fill_opacity = v as f32;
        }
        if let Some(v) = self.number("stroke-opacity")? {
            paint.stroke_opacity = v as f32;
        }
        if let Some(v) = self.length("stroke-width")? {
            paint.stroke_width = v;
        }
        if let Some(v) = self.number("stroke-miterlimit")? {
            paint.miter = v;
        }
        match self.get("fill-rule") {
            Some("evenodd") => paint.even_odd = true,
            Some("nonzero") => paint.even_odd = false,
            None => {}
            Some(_) => return None,
        }
        match self.get("stroke-linecap") {
            Some("butt") => paint.cap = Cap::Butt,
            Some("round") => paint.cap = Cap::Round,
            Some("square") => paint.cap = Cap::Square,
            None => {}
            Some(_) => return None,
        }
        match self.get("stroke-linejoin") {
            Some("miter") | Some("miter-clip") => paint.join = Join::Miter,
            Some("round") => paint.join = Join::Round,
            Some("bevel") => paint.join = Join::Bevel,
            None => {}
            Some(_) => return None,
        }
        match self.get("visibility") {
            Some("visible") => paint.visible = true,
            Some("hidden") | Some("collapse") => paint.visible = false,
            None => {}
            Some(_) => return None,
        }
        // Dashes are not supported here
        if self.get("stroke-dasharray").is_some_and(|v| v != "none") {
            return None;
        }
        if self
            .get("stroke-dashoffset")
            .is_some_and(|v| v.trim_end_matches("px").trim().parse::<f64>().ok() != Some(0.0))
        {
            return None;
        }
        Some(())
    }

    /// `none` → `Some(None)`; a solid color → `Some(Some(rgba))`; anything else (url(), context-fill ...) → `None`
    fn color(value: &str) -> Option<Option<Rgba>> {
        let value = value.trim();
        if value == "none" {
            return Some(None);
        }
        let c = svgtypes::Color::from_str(value).ok()?;
        Some(Some([
            f32::from(c.red) / 255.0,
            f32::from(c.green) / 255.0,
            f32::from(c.blue) / 255.0,
            f32::from(c.alpha) / 255.0,
        ]))
    }
}
