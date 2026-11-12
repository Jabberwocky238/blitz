//! SVG image data and CSS intrinsic sizing for SVG.

use std::sync::Arc;

use usvg::roxmltree;

/// Dimensions declared on the root `<svg>` element, before any resolution.
///
/// Unlike [`usvg::Tree::size`], which always produces a concrete size, this
/// preserves what the SVG actually declared: absent attributes are `None` and
/// percentage lengths are kept unresolved.
#[derive(Debug, Clone, Copy, Default)]
pub struct SvgIntrinsicDimensions {
    /// The root `width` attribute, if declared. Percentages are unresolved.
    pub width: Option<svgtypes::Length>,
    /// The root `height` attribute, if declared. Percentages are unresolved.
    pub height: Option<svgtypes::Length>,
    /// The root `viewBox`, if declared and valid.
    pub view_box: Option<usvg::NonZeroRect>,
    /// The root `preserveAspectRatio` (default `xMidYMid meet`).
    pub preserve_aspect_ratio: svgtypes::AspectRatio,
    /// Whether the root declared a `viewBox` with a zero width or height,
    /// which disables rendering of the element per the SVG spec.
    pub degenerate_view_box: bool,
}

impl SvgIntrinsicDimensions {
    /// Extract the `width`/`height`/`viewBox` attributes declared on the root
    /// element of an already-parsed SVG document, with absent attributes as
    /// `None` and percentages unresolved. [`usvg::Tree`] does not preserve
    /// these (it always resolves to a concrete size), so they are read from
    /// the XML document here.
    pub fn from_xmltree(doc: &roxmltree::Document) -> Self {
        let root = doc.root_element();

        let parse_length = |name: &str| -> Option<svgtypes::Length> {
            root.attribute(name)?.parse::<svgtypes::Length>().ok()
        };
        // Parsed manually rather than via `svgtypes::ViewBox`, which rejects
        // zero sizes: a zero `viewBox` width/height is distinguished from an
        // invalid `viewBox` as it disables rendering of the element.
        let view_box_dims = root.attribute("viewBox").and_then(|s| {
            let mut numbers = svgtypes::NumberListParser::from(s);
            let x = numbers.next()?.ok()? as f32;
            let y = numbers.next()?.ok()? as f32;
            let w = numbers.next()?.ok()? as f32;
            let h = numbers.next()?.ok()? as f32;
            (numbers.next().is_none() && w.is_finite() && h.is_finite() && w >= 0.0 && h >= 0.0)
                .then_some((x, y, w, h))
        });
        let view_box =
            view_box_dims.and_then(|(x, y, w, h)| usvg::NonZeroRect::from_xywh(x, y, w, h));
        let degenerate_view_box = view_box_dims.is_some_and(|(_, _, w, h)| w == 0.0 || h == 0.0);
        let preserve_aspect_ratio = root
            .attribute("preserveAspectRatio")
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();

        Self {
            width: parse_length("width"),
            height: parse_length("height"),
            view_box,
            preserve_aspect_ratio,
            degenerate_view_box,
        }
    }
}

/// A parsed SVG image.
///
/// usvg always resolves the root `<svg>` to a concrete [`usvg::Tree::size`],
/// falling back to the `viewBox` size when `width`/`height` are absent or given
/// as percentages. For CSS sizing purposes, however, such an SVG has *no*
/// intrinsic width/height (only an intrinsic aspect ratio). The accessors on
/// this type resolve the CSS intrinsic dimensions from the declared root
/// attributes, which are captured at parse time.
#[derive(Debug, Clone)]
pub struct SvgImageData {
    /// The parsed SVG tree. acrender: for a `simple` SVG this is only the root (size and viewBox).
    pub tree: Arc<usvg::Tree>,
    /// acrender: drawing commands for a simple SVG, painted instead of `tree` (see simple_svg.rs)
    pub simple: Option<Arc<super::simple_svg::SimpleSvg>>,
    /// The dimensions declared on the root `<svg>` element.
    pub intrinsic_dimensions: SvgIntrinsicDimensions,
}

impl SvgImageData {
    /// Parse an SVG image from raw data, capturing both the rendered
    /// [`usvg::Tree`] and the declared root dimensions from a single XML
    /// parse.
    ///
    /// Like [`usvg::Tree::from_data`], gzip-compressed data (SVGZ) is
    /// decompressed first.
    pub fn from_data(data: &[u8], options: &usvg::Options) -> Result<Self, usvg::Error> {
        // Gzip magic bytes, matching the SVGZ detection in `usvg::Tree::from_data`.
        let decompressed;
        let data = if data.starts_with(&[0x1f, 0x8b]) {
            decompressed = usvg::decompress_svgz(data)?;
            decompressed.as_slice()
        } else {
            data
        };

        let text = std::str::from_utf8(data).map_err(|_| usvg::Error::NotAnUtf8Str)?;
        let xml_options = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let doc = roxmltree::Document::parse_with_options(text, xml_options)
            .map_err(usvg::Error::ParsingFailed)?;
        let tree = usvg::Tree::from_xmltree(&doc, options)?;
        Ok(Self {
            tree: Arc::new(tree),
            simple: None,
            intrinsic_dimensions: SvgIntrinsicDimensions::from_xmltree(&doc),
        })
    }

    /// acrender: a simple SVG (see simple_svg.rs) without parsing it with usvg. `shell` parses just the
    /// root element (`width`/`height`/`viewBox`/`preserveAspectRatio`) for the size, and each `<text>` on
    /// its own; `parse` is the caller's cached usvg parse. `None` when the SVG is not simple.
    pub fn simple(
        source: &str,
        parse: impl Fn(&[u8]) -> Result<SvgImageData, usvg::Error>,
    ) -> Option<Self> {
        let doc = roxmltree::Document::parse(source).ok()?;
        let root = doc.root_element();
        let shell = Self::shell(|name| root.attribute(name), &parse)?;
        let size = shell.tree.size();
        let text = |mini: &str| parse(mini.as_bytes()).ok().map(|svg| svg.tree);
        let simple = super::simple_svg::SimpleSvg::parse(
            source,
            (size.width().into(), size.height().into()),
            &text,
        )?;
        Some(Self {
            simple: Some(Arc::new(simple)),
            ..shell
        })
    }

    /// acrender: [`Self::simple`] straight from the DOM (`node` is the root `<svg>`): the same pairs, without
    /// serializing and re-parsing the text. `None` when the SVG is not simple or might come out differently
    pub(crate) fn simple_dom(
        node: &crate::Node,
        viewport: (f32, f32),
        parse: impl Fn(&[u8]) -> Result<SvgImageData, usvg::Error>,
    ) -> Option<Self> {
        let element = node.svg_element(true, viewport)?;
        let attr = |name: &str| {
            let (_, v) = element.attrs.iter().find(|(n, _)| n == name)?;
            Some(v.as_str())
        };
        let shell = Self::shell(attr, &parse)?;
        let size = shell.tree.size();
        let text = |mini: &str| parse(mini.as_bytes()).ok().map(|svg| svg.tree);
        let simple = super::simple_svg::SimpleSvg::from_dom(
            node,
            viewport,
            (size.width().into(), size.height().into()),
            &text,
        )?;
        Some(Self {
            simple: Some(Arc::new(simple)),
            ..shell
        })
    }

    /// acrender: the root element alone (the attributes that size it), parsed with `parse`
    fn shell<'v>(
        attr: impl Fn(&str) -> Option<&'v str>,
        parse: impl Fn(&[u8]) -> Result<SvgImageData, usvg::Error>,
    ) -> Option<Self> {
        let mut root_only = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"");
        for name in ["width", "height", "viewBox", "preserveAspectRatio"] {
            if let Some(v) = attr(name) {
                root_only.push_str(&format!(
                    " {name}=\"{}\"",
                    v.replace('&', "&amp;")
                        .replace('"', "&quot;")
                        .replace('<', "&lt;")
                ));
            }
        }
        root_only.push_str("/>");
        parse(root_only.as_bytes()).ok()
    }

    /// The intrinsic width in CSS px, present only when the root `<svg>`
    /// declared an absolute (non-percentage) `width`.
    pub fn intrinsic_width(&self) -> Option<f32> {
        use svgtypes::LengthUnit;
        let declared = self
            .intrinsic_dimensions
            .width
            .is_some_and(|len| len.unit != LengthUnit::Percent);
        declared.then(|| self.tree.size().width())
    }

    /// The intrinsic height in CSS px, present only when the root `<svg>`
    /// declared an absolute (non-percentage) `height`.
    pub fn intrinsic_height(&self) -> Option<f32> {
        use svgtypes::LengthUnit;
        let declared = self
            .intrinsic_dimensions
            .height
            .is_some_and(|len| len.unit != LengthUnit::Percent);
        declared.then(|| self.tree.size().height())
    }

    /// The aspect ratio of the root `<svg>`'s `viewBox`, if it declares one.
    pub fn viewbox_aspect_ratio(&self) -> Option<f32> {
        self.intrinsic_dimensions
            .view_box
            .map(|view_box| view_box.width() / view_box.height())
    }

    /// The transform from the rendered tree (in [`usvg::Tree::size`] units) to a
    /// viewport of the given size, applying the root `viewBox` and
    /// `preserveAspectRatio`. Without a `viewBox` the tree is stretched to fill it.
    pub fn viewport_transform(&self, width: f32, height: f32) -> usvg::Transform {
        let size = self.tree.size();
        let stretch = usvg::Transform::from_scale(width / size.width(), height / size.height());
        let Some(view_box) = self.intrinsic_dimensions.view_box else {
            return stretch;
        };
        let aspect = self.intrinsic_dimensions.preserve_aspect_ratio;
        // usvg has already mapped the `viewBox` onto `Tree::size`, so undo that first.
        let view_box_to_tree = view_box_transform(view_box, aspect, size.width(), size.height());
        match view_box_to_tree.invert() {
            Some(tree_to_view_box) => {
                view_box_transform(view_box, aspect, width, height).pre_concat(tree_to_view_box)
            }
            None => stretch,
        }
    }

    /// acrender: the transform from the rendered tree to the content box (`width` x `height`) of an inline `<svg>`. Without
    /// a `viewBox` its user units are CSS pixels from the box's origin, as in browsers (an image's SVG is stretched to its
    /// box instead, see [`Self::viewport_transform`]); with one, the same as [`Self::viewport_transform`].
    pub fn inline_viewport_transform(&self, width: f32, height: f32) -> usvg::Transform {
        match self.intrinsic_dimensions.view_box {
            None => usvg::Transform::identity(),
            Some(_) => self.viewport_transform(width, height),
        }
    }

    /// The root `width` attribute resolved against a containing block width:
    /// percentages resolve against the containing block (`None` if it is
    /// indefinite) and an absent attribute is `None`.
    ///
    /// This is only appropriate for an inline `<svg>` element, where the
    /// attributes behave as presentation attributes. SVG used as an image
    /// (e.g. `<img src>` or a background) must use [`Self::intrinsic_width`],
    /// as its intrinsic dimensions are context-free per CSS.
    pub fn resolved_width(&self, container_width: Option<f32>) -> Option<f32> {
        use svgtypes::LengthUnit;
        match self.intrinsic_dimensions.width {
            Some(len) if len.unit != LengthUnit::Percent => Some(self.tree.size().width()),
            Some(len) => container_width.map(|cw| cw * (len.number as f32) / 100.0),
            None => None,
        }
    }

    /// The root `height` attribute resolved against a containing block height.
    /// See [`Self::resolved_width`].
    pub fn resolved_height(&self, container_height: Option<f32>) -> Option<f32> {
        use svgtypes::LengthUnit;
        match self.intrinsic_dimensions.height {
            Some(len) if len.unit != LengthUnit::Percent => Some(self.tree.size().height()),
            Some(len) => container_height.map(|ch| ch * (len.number as f32) / 100.0),
            None => None,
        }
    }

    /// The intrinsic aspect ratio of the SVG: the ratio of its declared
    /// `width`/`height` when both are absolute lengths, otherwise the
    /// `viewBox` ratio, otherwise the ratio of the resolved
    /// [`usvg::Tree::size`] (which is always non-zero).
    pub fn aspect_ratio(&self) -> f32 {
        match (self.intrinsic_width(), self.intrinsic_height()) {
            (Some(w), Some(h)) => w / h,
            _ => self.viewbox_aspect_ratio().unwrap_or_else(|| {
                let size = self.tree.size();
                size.width() / size.height()
            }),
        }
    }

    /// The intrinsic dimensions of the SVG resolved per CSS replaced element
    /// sizing: a missing dimension is computed from the declared one and the
    /// intrinsic aspect ratio; if neither is declared, the resolved
    /// [`usvg::Tree::size`] is used as a fallback.
    pub fn intrinsic_size(&self) -> (f32, f32) {
        let aspect_ratio = self.aspect_ratio();
        match (self.intrinsic_width(), self.intrinsic_height()) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, w / aspect_ratio),
            (None, Some(h)) => (h * aspect_ratio, h),
            (None, None) => {
                // No intrinsic dimensions. If there is an intrinsic aspect ratio, apply
                // the CSS default sizing algorithm: contain within the default object
                // size of 300x150. Otherwise fall back to the resolved tree size.
                if self.viewbox_aspect_ratio().is_some() {
                    let scale = (300.0 / aspect_ratio).min(150.0);
                    (scale * aspect_ratio, scale)
                } else {
                    let size = self.tree.size();
                    (size.width(), size.height())
                }
            }
        }
    }
}

/// Map a `viewBox` onto a viewport of the given size per `preserveAspectRatio`.
fn view_box_transform(
    view_box: usvg::NonZeroRect,
    aspect: svgtypes::AspectRatio,
    width: f32,
    height: f32,
) -> usvg::Transform {
    use svgtypes::Align;

    let sx = width / view_box.width();
    let sy = height / view_box.height();
    let (sx, sy) = if aspect.align == Align::None {
        (sx, sy)
    } else if aspect.slice {
        (sx.max(sy), sx.max(sy))
    } else {
        (sx.min(sy), sx.min(sy))
    };
    let (align_x, align_y) = match aspect.align {
        Align::None | Align::XMinYMin => (0.0, 0.0),
        Align::XMidYMin => (0.5, 0.0),
        Align::XMaxYMin => (1.0, 0.0),
        Align::XMinYMid => (0.0, 0.5),
        Align::XMidYMid => (0.5, 0.5),
        Align::XMaxYMid => (1.0, 0.5),
        Align::XMinYMax => (0.0, 1.0),
        Align::XMidYMax => (0.5, 1.0),
        Align::XMaxYMax => (1.0, 1.0),
    };
    let tx = (width - view_box.width() * sx) * align_x - view_box.x() * sx;
    let ty = (height - view_box.height() * sy) * align_y - view_box.y() * sy;
    usvg::Transform::from_row(sx, 0.0, 0.0, sy, tx, ty)
}

const XLINK: &str = "http://www.w3.org/1999/xlink";

fn parse_xml(text: &str) -> Option<roxmltree::Document<'_>> {
    let options = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    roxmltree::Document::parse_with_options(text, options).ok()
}

/// acrender: the `href`s of `<use>` elements that point into another file (`icons.svg#github`)
pub(crate) fn external_uses(source: &str) -> Vec<String> {
    let Some(doc) = parse_xml(source) else {
        return Vec::new();
    };
    let mut hrefs: Vec<String> = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("use")) {
        let Some(href) = node.attribute("href").or_else(|| node.attribute((XLINK, "href"))) else {
            continue;
        };
        let external = href.split_once('#').is_some_and(|(path, id)| !path.is_empty() && !id.is_empty());
        if external && !hrefs.iter().any(|known| known == href) {
            hrefs.push(href.to_owned());
        }
    }
    hrefs
}

/// acrender: the ids `source` refers to (`href="#id"`, `xlink:href="#id"`, `url(#id)`) that it does not define
pub(crate) fn missing_local_refs(source: &str) -> Vec<String> {
    let defined = |id: &str| source.contains(&format!("id=\"{id}\"")) || source.contains(&format!("id='{id}'"));
    let mut refs: Vec<String> = Vec::new();
    let mut add = |id: &str| {
        let id = id.trim();
        if !id.is_empty() && !defined(id) && !refs.iter().any(|known| known == id) {
            refs.push(id.to_owned());
        }
    };
    for (pattern, end) in [("href=\"#", '"'), ("href='#", '\''), ("url(#", ')'), ("url(\"#", '"'), ("url('#", '\'')] {
        let mut rest = source;
        while let Some(at) = rest.find(pattern) {
            rest = &rest[at + pattern.len()..];
            if let Some(stop) = rest.find(end) {
                add(&rest[..stop]);
            }
        }
    }
    refs
}

/// acrender: usvg resolves `<use>` only inside the same document; copy each element an external `<use>`
/// points at (from `sprite(path)`, the loaded file) into a `<defs>` of `source` under a local id
pub(crate) fn inline_external_uses(source: &str, sprite: impl Fn(&str) -> Option<Arc<str>>) -> String {
    let mut out = source.to_owned();
    let mut defs = String::new();
    for (n, href) in external_uses(source).iter().enumerate() {
        let Some((path, id)) = href.split_once('#') else {
            continue;
        };
        let Some(text) = sprite(path) else {
            continue;
        };
        let Some(external) = parse_xml(&text) else {
            continue;
        };
        let Some(target) = external.descendants().find(|node| node.attribute("id") == Some(id)) else {
            continue;
        };
        let local = format!("acde-sprite-{n}-{id}");
        let piece = &text[target.range()];
        let renamed = piece
            .replacen(&format!("id=\"{id}\""), &format!("id=\"{local}\""), 1)
            .replacen(&format!("id='{id}'"), &format!("id=\"{local}\""), 1);
        defs.push_str(&renamed);
        let quoted = format!("\"{}\"", href.replace('&', "&amp;"));
        out = out.replace(&quoted, &format!("\"#{local}\""));
    }
    if let (false, Some(end)) = (defs.is_empty(), out.rfind("</svg>")) {
        out.insert_str(end, &format!("<defs>{defs}</defs>"));
    }
    out
}

#[cfg(test)]
mod sprite_tests {
    use super::*;

    const PAGE: &str = concat!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">"#,
        r##"<use href="/icons.svg#github"></use><use xlink:href="#local"></use></svg>"##
    );
    const SPRITE: &str = concat!(
        r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg">"#,
        r#"<symbol id="github" viewBox="0 0 16 16"><path d="M0 0h16v16z"/></symbol><symbol id="x"/></svg>"#
    );

    #[test]
    fn only_uses_into_other_files_are_external() {
        assert_eq!(external_uses(PAGE), ["/icons.svg#github"]);
        assert!(external_uses("<svg xmlns=\"http://www.w3.org/2000/svg\"/>").is_empty());
    }

    #[test]
    fn the_target_is_copied_in_under_a_local_id() {
        let inlined = inline_external_uses(PAGE, |path| (path == "/icons.svg").then(|| Arc::from(SPRITE)));
        assert!(inlined.contains(r##"<use href="#acde-sprite-0-github">"##), "{inlined}");
        assert!(inlined.contains(r#"<defs><symbol id="acde-sprite-0-github" viewBox="0 0 16 16"><path d="M0 0h16v16z"/></symbol></defs></svg>"#), "{inlined}");
        assert!(!inlined.contains("symbol id=\"x\""));
        assert!(usvg::Tree::from_str(&inlined, &usvg::Options::default()).is_ok());
        assert_eq!(inline_external_uses(PAGE, |_| None), PAGE);
    }

    #[test]
    fn references_this_svg_does_not_define() {
        let source = r##"<svg><use href="#a"/><use xlink:href='#b'/><rect fill="url(#g)" stroke="url('#h')"/><g id="a"/></svg>"##;
        assert_eq!(missing_local_refs(source), ["b", "g", "h"]);
    }
}
