use crate::node::{Node, NodeData};
use color::{AlphaColor, Srgb};
use std::borrow::Cow;
use style::color::AbsoluteColor;

pub type Color = AlphaColor<Srgb>;

/// Decode raw font bytes, decompressing WOFF/WOFF2 if the `woff` feature is enabled.
/// Returns the original slice unchanged for TTF/OTF input, and also on decompression
/// failure. With the `woff` feature disabled, all input passes through unchanged.
pub fn decode_font_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    if bytes.len() < 4 {
        return Cow::Borrowed(bytes);
    }
    match &bytes[0..4] {
        #[cfg(feature = "woff")]
        b"wOFF" => wuff::decompress_woff1(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff1 font");
                Cow::Borrowed(bytes)
            }),
        #[cfg(feature = "woff")]
        b"wOF2" => wuff::decompress_woff2(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff2 font");
                Cow::Borrowed(bytes)
            }),
        _ => Cow::Borrowed(bytes),
    }
}

#[cfg(feature = "svg")]
use std::sync::{Arc, LazyLock};
#[cfg(feature = "svg")]
use usvg::fontdb;
#[cfg(feature = "svg")]
pub(crate) static FONT_DB: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    // acrender: fontdb's generic families default to Windows fonts ("Courier New" ...); when those are not
    // installed usvg drops the whole <text> ("No match for Geist Mono, ui-monospace, monospace").
    // Use the families fontconfig picks (via fontique, as for HTML text).
    use parley::fontique::{Collection, CollectionOptions, GenericFamily};
    let mut collection = Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    });
    let mut name = |g| {
        let id = collection.generic_families(g).next()?;
        collection.family_name(id).map(str::to_owned)
    };
    if let Some(f) = name(GenericFamily::Monospace) {
        db.set_monospace_family(f);
    }
    if let Some(f) = name(GenericFamily::SansSerif) {
        db.set_sans_serif_family(f);
    }
    if let Some(f) = name(GenericFamily::Serif) {
        db.set_serif_family(f);
    }
    if let Some(f) = name(GenericFamily::Cursive) {
        db.set_cursive_family(f);
    }
    if let Some(f) = name(GenericFamily::Fantasy) {
        db.set_fantasy_family(f);
    }
    Arc::new(db)
});

/// acrender: parsed inline SVGs by source text. An inline <svg> is re-serialized and re-parsed on every
/// style change in its subtree (an animated Dopa face: every frame); the same source parses to the
/// same tree, so reuse it. Bounded: cleared when it grows past this many entries.
#[cfg(feature = "svg")]
const SVG_CACHE_LIMIT: usize = 512;
/// acrender: SVG source (+ font database identity) -> parsed SVG and its estimated bytes
#[cfg(feature = "svg")]
type SvgCache = std::collections::HashMap<Vec<u8>, (crate::node::SvgImageData, usize)>;
#[cfg(feature = "svg")]
static SVG_CACHE: LazyLock<std::sync::Mutex<SvgCache>> = LazyLock::new(Default::default);

/// Which kind of CSS image layer list (`background-image` or `mask-image`) to
/// flush from style to dedicated storage on the node.
#[derive(Clone, Copy, Debug)]
pub enum ImageLayerKind {
    Background,
    Mask,
}

impl ImageLayerKind {
    pub fn image_type(self, idx: usize) -> ImageType {
        match self {
            Self::Background => ImageType::Background(idx),
            Self::Mask => ImageType::Mask(idx),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ImageType {
    Image,
    Background(usize),
    Mask(usize),
}

/// A point
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub struct Point<T> {
    /// The x coordinate
    pub x: T,
    /// The y coordinate
    pub y: T,
}

impl Point<f64> {
    pub const ZERO: Self = Point { x: 0.0, y: 0.0 };
}

// Debug print an RcDom
pub fn walk_tree(indent: usize, node: &Node) {
    // Skip all-whitespace text nodes entirely
    if let NodeData::Text(data) = &node.data {
        if data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    print!("{}", " ".repeat(indent));
    let id = node.id;
    match &node.data {
        NodeData::Document(_) => println!("#Document {id}"),

        NodeData::Text(data) => {
            if data.content.chars().all(|c| c.is_ascii_whitespace()) {
                println!("{id} #text: <whitespace>");
            } else {
                let content = data.content.trim();
                if content.len() > 10 {
                    println!(
                        "#text {id}: {}...",
                        content
                            .split_at(content.char_indices().take(10).last().unwrap().0)
                            .0
                            .escape_default()
                    )
                } else {
                    println!("#text {id}: {}", data.content.trim().escape_default())
                }
            }
        }

        NodeData::Comment { .. } => println!("<!-- COMMENT {id} -->"),

        NodeData::AnonymousBlock(_) => println!("{id} AnonymousBlock"),

        NodeData::Element(data) => {
            print!("<{} {id}", data.name.local);
            for attr in data.attrs.iter() {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            if !node.children.is_empty() {
                println!(">");
            } else {
                println!("/>");
            }
        } // NodeData::Doctype {
          //     ref name,
          //     ref public_id,
          //     ref system_id,
          // } => println!("<!DOCTYPE {} \"{}\" \"{}\">", name, public_id, system_id),
          // NodeData::ProcessingInstruction { .. } => unreachable!(),
    }

    if !node.children.is_empty() {
        for child_id in node.children.iter() {
            walk_tree(indent + 2, node.with(*child_id));
        }

        if let NodeData::Element(data) = &node.data {
            println!("{}</{}>", " ".repeat(indent), data.name.local);
        }
    }
}

/// Parse an SVG image.
/// acrender: fonts: the document's database for SVG `<text>` (see BaseDocument::svg_fonts); None: the system one
#[cfg(feature = "svg")]
pub(crate) fn parse_svg_image(
    source: &[u8],
    fonts: Option<Arc<fontdb::Database>>,
) -> Result<crate::node::SvgImageData, usvg::Error> {
    // acrender: simple SVGs skip usvg (see simple_svg.rs); their root-only "shell" goes through the cache too
    let simple = std::str::from_utf8(source).ok().and_then(|text| {
        crate::node::SvgImageData::simple(text, |s| parse_svg_with(s, fonts.clone()))
    });
    // acrender: ACRENDER_SVG_PATH=1 prints which path each inline SVG took (to check what stays on usvg)
    if std::env::var_os("ACRENDER_SVG_PATH").is_some() {
        let head = String::from_utf8_lossy(&source[..source.len().min(120)]);
        eprintln!(
            "acrender svg path: {} {} bytes {head}",
            if simple.is_some() { "simple" } else { "usvg" },
            source.len()
        );
    }
    match simple {
        Some(svg) => Ok(svg),
        None => parse_svg_with(source, fonts),
    }
}

/// acrender: entries in SVG_CACHE and their estimated bytes (source text + parsed tree, see svg_bytes)
#[cfg(feature = "svg")]
pub(crate) fn svg_cache_stats() -> (usize, usize) {
    SVG_CACHE.lock().map_or((0, 0), |c| {
        (c.len(), c.iter().map(|(k, (_, b))| k.len() + b).sum())
    })
}

#[cfg(not(feature = "svg"))]
pub(crate) fn svg_cache_stats() -> (usize, usize) {
    (0, 0)
}

/// acrender: rough size of a parsed SVG: nodes and path points of the usvg tree, simple-SVG ops
#[cfg(feature = "svg")]
fn svg_bytes(svg: &crate::node::SvgImageData) -> usize {
    fn group(g: &usvg::Group) -> usize {
        g.children()
            .iter()
            .map(|n| match n {
                usvg::Node::Group(g) => 256 + group(g),
                usvg::Node::Path(p) => 256 + p.data().points().len() * 8 + p.data().verbs().len(),
                usvg::Node::Image(_) | usvg::Node::Text(_) => 1024,
            })
            .sum()
    }
    group(svg.tree.root()) + svg.simple.as_ref().map_or(0, |s| s.ops.len() * 128)
}

/// usvg parse, cached by source text (see SVG_CACHE)
/// acrender: usvg only needs fonts for <text>. An SVG without text gets an empty database; with text, the
/// document's (fonts) or else FONT_DB, which loads every system font file (~70 ms of CPU and the reads
/// of a few hundred files on a cold start). The cache key says which database the tree was made with.
#[cfg(feature = "svg")]
pub(crate) fn parse_svg_with(
    source: &[u8],
    fonts: Option<Arc<fontdb::Database>>,
) -> Result<crate::node::SvgImageData, usvg::Error> {
    let text = has_text_element(source);
    let mut key = source.to_vec();
    if let Some(db) = fonts.as_ref().filter(|_| text) {
        key.extend(b"\0fonts:");
        key.extend((Arc::as_ptr(db) as usize).to_le_bytes());
        key.extend(db.len().to_le_bytes());
    }
    // ACRENDER_SVG_CACHE=0: no cache (A/B for memory growth)
    let enabled = std::env::var_os("ACRENDER_SVG_CACHE").is_none_or(|v| v != "0");
    if let Some(hit) = SVG_CACHE
        .lock()
        .ok()
        .filter(|_| enabled)
        .and_then(|c| c.get(&key).map(|(s, _)| s.clone()))
    {
        return Ok(hit);
    }
    let fontdb = match (text, fonts) {
        (false, _) => Arc::new(fontdb::Database::new()),
        (true, Some(db)) => db,
        (true, None) => Arc::clone(&*FONT_DB),
    };
    let options = usvg::Options {
        fontdb,
        ..Default::default()
    };
    let parsed = crate::node::SvgImageData::from_data(source, &options)?;
    if let Ok(mut cache) = SVG_CACHE
        .lock()
        .map_err(|_| ())
        .and_then(|c| if enabled { Ok(c) } else { Err(()) })
    {
        if cache.len() >= SVG_CACHE_LIMIT {
            cache.clear();
        }
        let bytes = svg_bytes(&parsed);
        cache.insert(key, (parsed.clone(), bytes));
    }
    Ok(parsed)
}

/// acrender: whether an SVG source has a `<text>` (also `<svg:text>`) element
#[cfg(feature = "svg")]
pub(crate) fn has_text_element(source: &[u8]) -> bool {
    source
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"text")
        .any(|(i, _)| {
            let before = i.checked_sub(1).map(|j| source[j]);
            let after = source.get(i + 4).copied();
            matches!(before, Some(b'<' | b':'))
                && after.is_none_or(|c| c.is_ascii_whitespace() || c == b'>' || c == b'/')
        })
}

pub trait ToColorColor {
    /// Converts a color into the `AlphaColor<Srgb>` type from the `color` crate
    fn as_color_color(&self) -> Color;
}
impl ToColorColor for AbsoluteColor {
    fn as_color_color(&self) -> Color {
        Color::new(
            *self
                .to_color_space(style::color::ColorSpace::Srgb)
                .raw_components(),
        )
    }
}

#[cfg(all(test, feature = "svg"))]
mod svg_tests {
    use super::parse_svg_image;

    #[test]
    fn svg_without_xmlns_parses() {
        let src = br#"<svg viewBox="0 0 24 24"><path d="M0 0h24v24H0z"/></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        // acrender: a simple SVG keeps its drawing commands in `simple`, not in the usvg tree
        assert!(svg.simple.is_some() || !svg.tree.root().children().is_empty());
    }

    #[test]
    fn missing_height_is_computed_from_width_and_viewbox_ratio() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="200"><rect width="100%" height="100%" fill="green"/></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(200.0));
        assert_eq!(svg.intrinsic_height(), None);
        assert_eq!(svg.viewbox_aspect_ratio(), Some(1.0));
        assert_eq!(svg.tree.size().width(), 200.0);
        assert_eq!(svg.intrinsic_size(), (200.0, 200.0));
    }

    #[test]
    fn viewbox_only_has_no_intrinsic_dimensions() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 485 58"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
        // The aspect ratio is still available from the viewBox.
        assert!((svg.aspect_ratio() - (485.0 / 58.0)).abs() < 1e-3);
    }

    #[test]
    fn absolute_dimensions_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="16" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(24.0));
        assert_eq!(svg.intrinsic_height(), Some(16.0));
    }

    #[test]
    fn percentage_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="50%" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }

    #[test]
    fn unit_lengths_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24px" height="1.5em" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert!(svg.intrinsic_width().is_some());
        assert!(svg.intrinsic_height().is_some());
    }

    #[test]
    fn non_numeric_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="auto" height="foo" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }
}

/// Creates an markup5ever::QualName.
/// Given a local name and an optional namespace
#[macro_export]
macro_rules! qual_name {
    ($local:tt $(, $ns:ident)?) => {
        $crate::QualName {
            prefix: None,
            ns: $crate::ns!($($ns)?),
            local: $crate::local_name!($local),
        }
    };
}
