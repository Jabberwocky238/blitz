//! Paint a [`blitz_dom::BaseDocument`] by pushing drawing commands into an impl of
//! [`acde_gpu::PaintScene`] (acrender: the 2D scene is acdert's acde-gpu, anyrender is gone).

#![allow(clippy::collapsible_if)]

mod cache;
mod color;
mod filters;
mod gradient;
mod kurbo_css;
mod layers;
mod render;
mod sizing;
#[cfg(feature = "svg")]
mod svg;
mod text;

use std::collections::HashSet;

use acde_gpu::{PaintScene, Scene};
use blitz_dom::{BaseDocument, NodeId, util::Color};
use render::BlitzDomPainter;

pub use cache::{CacheStats, Change, PaintCache, Piece, Run};
pub use render::placed;

const FONT_EMBOLDEN_ENABLED: bool = cfg!(any(
    feature = "font-embolden",
    all(feature = "apple-font-embolden", target_os = "macos"),
    all(feature = "apple-font-embolden", target_os = "ios"),
));

/// The default color for text selection highlights
const SELECTION_COLOR: Color = Color::from_rgb8(180, 213, 255);

/// Paint a [`blitz_dom::BaseDocument`] by pushing drawing commands into
/// an impl [`acde_gpu::PaintScene`].
///
/// This function assumes that the styles and layout in the [`BaseDocument`] are already
/// resolved. Please ensure that this is the case before trying to paint.
///
/// The implementation of [`PaintScene`] is responsible for handling the commands that are pushed into it.
/// Generally this will involve executing them to draw a rasterized image/texture. But in some cases it may choose to
/// transform them to a vector format (e.g. SVG/PDF) or serialize them in raw form for later use.
pub fn paint_scene(
    scene: &mut impl PaintScene,
    doc: &mut BaseDocument,
    scale: f64,
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
) {
    let generator =
        BlitzDomPainter::new(doc, scale, width, height, x_offset as f64, y_offset as f64);
    // acrender: the painter records into a Scene (fragments are cut from it): record, then replay into `scene`
    let mut recorded = Scene::new();
    generator.paint_scene(&mut recorded);
    scene.append_scene(recorded, kurbo::Affine::IDENTITY);

    // println!(
    //     "Rendered using {} clips (depth: {}) (wanted: {})",
    //     CLIPS_USED.load(atomic::Ordering::SeqCst),
    //     CLIP_DEPTH_USED.load(atomic::Ordering::SeqCst),
    //     CLIPS_WANTED.load(atomic::Ordering::SeqCst)
    // );
}

/// acrender: [`paint_scene`] through the fragment cache: only fragments whose inputs changed since
/// the last frame are re-recorded (see `cache`). Elements are culled to the viewport only (not to
/// ancestor scrollports). The frame is `scene` (what is painted outside fragments, first) followed
/// by `pieces`, slices of the cached fragments. `pinned` elements (and their subtrees) are left out
/// of the frame and returned on their own, as slices placed in the document: each is a fragment of
/// its own even when it is not a boundary (the cache is to be cleared when the set changes).
#[allow(clippy::too_many_arguments)]
pub fn paint_scene_cached(
    scene: &mut Scene,
    pieces: &mut Vec<Piece>,
    doc: &mut BaseDocument,
    (scale, width, height): (f64, u32, u32),
    cache: &mut PaintCache,
    canvas: bool,
    pinned: &HashSet<NodeId>,
) -> Vec<(NodeId, Vec<Piece>)> {
    let cell = std::cell::RefCell::new(std::mem::take(cache));
    let painted = paint_viewport(
        scene,
        doc,
        (scale, width, height),
        Some(&cell),
        canvas,
        pinned,
    );
    *cache = cell.into_inner();
    *pieces = painted.0;
    painted.1
}

/// acrender: [`paint_scene`] without the cache, culling like it does: what [`paint_scene_cached`] must
/// reproduce (its verify mode compares the two).
pub fn paint_scene_reference(
    scene: &mut Scene,
    doc: &mut BaseDocument,
    (scale, width, height): (f64, u32, u32),
    canvas: bool,
    pinned: &HashSet<NodeId>,
) {
    paint_viewport(scene, doc, (scale, width, height), None, canvas, pinned);
}

/// acrender: what [`paint_scene_cached`] and [`paint_scene_reference`] share
#[allow(clippy::type_complexity)]
fn paint_viewport(
    scene: &mut Scene,
    doc: &mut BaseDocument,
    (scale, width, height): (f64, u32, u32),
    cache: Option<&std::cell::RefCell<PaintCache>>,
    canvas: bool,
    pinned: &HashSet<NodeId>,
) -> (Vec<Piece>, Vec<(NodeId, Vec<Piece>)>) {
    let mut painter = BlitzDomPainter::new(doc, scale, width, height, 0.0, 0.0);
    painter.cache = cache;
    painter.canvas = canvas;
    painter.scrollport_cull = false;
    painter.pinned = pinned.clone();
    painter.paint_scene(scene);
    (painter.pieces.take(), painter.pinned_pieces.take())
}

/// acrender: an open drop-down box's list of options (see [`blitz_dom::node::SelectPicker`]), to draw
/// on top of the page
pub fn paint_select_picker(scene: &mut Scene, doc: &BaseDocument, picker: &blitz_dom::node::SelectPicker, scale: f64) {
    render::form_controls::draw_picker(scene, doc, picker, scale);
}

/// acrender: paints one element and its subtree (e.g. for a drag image), see
/// [`BlitzDomPainter::paint_element`].
pub fn paint_element(
    scene: &mut Scene,
    doc: &BaseDocument,
    node_id: NodeId,
    scale: f64,
    transform: kurbo::Affine,
) {
    let (width, height) = doc.viewport().window_size;
    BlitzDomPainter::new(doc, scale, width, height, 0.0, 0.0)
        .paint_element(scene, node_id, transform);
}
