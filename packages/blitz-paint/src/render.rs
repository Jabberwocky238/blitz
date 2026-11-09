mod background;
mod border;
mod box_shadow;
mod clip_path;
pub(crate) mod form_controls;
mod mask;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::kurbo_css::CssBox;
use crate::cache::{Builder, PaintCache, Piece, Ref, Run};
use crate::color::ToColorColor;
// acrender: the scrollbar thumbs (feature `scrollbars`, on in acde-base) need these
use crate::SELECTION_COLOR;
#[cfg(feature = "scrollbars")]
use crate::color::Color;
use crate::filters::convert_filters;
use crate::kurbo_css::NonUniformRoundedRectRadii;
use crate::layers;
use crate::sizing::compute_object_fit;
use crate::text::DrawTextContext;
use acde_gpu::{PaintScene, Scene};
use blitz_dom::node::{
    ListItemLayout, ListItemLayoutPosition, Marker, NodeData, RasterImageData, TextInputData,
    TextLayout, TextNodeData,
};
use blitz_dom::{BaseDocument, ElementData, Node, NodeId, local_name};
#[cfg(feature = "scrollbars")]
use kurbo::Stroke;

use style::values::computed::{BorderCornerRadius, ColorOrAuto};
use style::{
    dom::TElement,
    properties::{
        ComputedValues, generated::longhands::visibility::computed_value::T as StyloVisibility,
        style_structs::Font,
    },
    values::{
        computed::{CSSPixelLength, Contain, Overflow},
        specified::box_::{DisplayInside, DisplayOutside},
        specified::image::ImageRendering,
    },
};

use kurbo::{self, Affine, Insets, Point, Rect, Size, Vec2};
use peniko::{self, Fill, ImageData, ImageSampler};
use taffy::Layout;

/// A view of the track positions reported by taffy in physical (left-to-right /
/// top-to-bottom) order. Taffy reports tracks in logical order, which for RTL grid
/// containers is physically reversed.
#[derive(Clone, Copy)]
pub(crate) struct PhysicalTracks<'a> {
    positions: &'a [taffy::Line<f32>],
    reversed: bool,
}

impl<'a> PhysicalTracks<'a> {
    pub(crate) fn from_tracks<S: taffy::CheapCloneStr>(
        tracks: &'a taffy::DetailedGridTracksInfo<S>,
    ) -> Self {
        let positions = tracks.positions.as_slice();
        let reversed = positions
            .first()
            .zip(positions.last())
            .is_some_and(|(first, last)| first.start > last.start);
        Self {
            positions,
            reversed,
        }
    }

    fn get(self, index: usize) -> taffy::Line<f32> {
        if self.reversed {
            self.positions[self.positions.len() - 1 - index]
        } else {
            self.positions[index]
        }
    }

    /// Iterate track positions in physical order
    pub(crate) fn iter(self) -> impl ExactSizeIterator<Item = taffy::Line<f32>> + 'a {
        (0..self.positions.len()).map(move |index| self.get(index))
    }

    /// The physical start position of the first track
    pub(crate) fn origin(self) -> f32 {
        if self.positions.is_empty() {
            0.0
        } else {
            self.get(0).start
        }
    }

    /// Total distance from the start of the first track to the end of the last track
    pub(crate) fn span(self) -> f32 {
        if self.positions.is_empty() {
            0.0
        } else {
            self.get(self.positions.len() - 1).end - self.get(0).start
        }
    }
}

/// acrender: `command` with `transform` applied on top of its own
pub fn placed(command: acde_gpu::RenderCommand, transform: Affine) -> acde_gpu::RenderCommand {
    command.transformed(transform)
}

/// A short-lived struct which holds a bunch of parameters for rendering a scene so
/// that we don't have to pass them down as parameters
pub struct BlitzDomPainter<'dom, 'a> {
    /// Input parameters (read only) for generating the Scene
    pub(crate) dom: &'dom BaseDocument,
    pub(crate) scale: f64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) initial_x: f64,
    pub(crate) initial_y: f64,
    /// The id of the document's root element (cached to avoid re-resolving it for every element)
    pub(crate) root_element_id: Option<NodeId>,
    /// Scrollbar hover/drag state, resolved once per scene like the root element
    #[cfg(feature = "scrollbars")]
    pub(crate) hovered_scrollbar: Option<blitz_dom::node::ScrollbarRef>,
    #[cfg(feature = "scrollbars")]
    pub(crate) scrollbar_drag_target: Option<blitz_dom::node::ScrollbarRef>,
    /// Reusable scratch allocations shared by all text layouts painted for this document.
    pub(crate) draw_text_context: RefCell<DrawTextContext>,
    /// Cached selection ranges for O(1) lookup: node_id -> (start_offset, end_offset)
    pub(crate) selection_ranges: HashMap<NodeId, (usize, usize)>,

    /// acrender: fragment cache (see `cache`); `None` paints uncached
    pub(crate) cache: Option<&'a RefCell<PaintCache>>,
    /// acrender: fragments being recorded, innermost last
    pub(crate) recording: RefCell<Vec<Builder>>,
    /// acrender: the element whose background is painted as the canvas background (and not again)
    pub(crate) canvas_background: Option<NodeId>,
    /// acrender: narrow the cull rectangle to ancestor scrollports (off for cached / reference painting)
    pub(crate) scrollport_cull: bool,
    /// acrender: paint the canvas background (the part that goes at the bottom)
    pub(crate) canvas: bool,
    /// acrender: the cached paint's frame, as slices of the fragments (see `flatten`)
    pub(crate) pieces: RefCell<Vec<Piece>>,
    /// acrender: elements painted on their own, not in the frame (see `paint_scene_cached`)
    pub(crate) pinned: HashSet<NodeId>,
    /// acrender: each pinned element's slices, at its placement in the document
    pub(crate) pinned_pieces: RefCell<Vec<(NodeId, Vec<Piece>)>>,
}

impl<'dom, 'a> BlitzDomPainter<'dom, 'a> {
    /// Create a new BlitzDomPainter for the given document
    pub fn new(
        dom: &'dom BaseDocument,
        scale: f64,
        width: u32,
        height: u32,
        initial_x: f64,
        initial_y: f64,
    ) -> Self {
        let selection_ranges: HashMap<NodeId, (usize, usize)> = dom
            .get_text_selection_ranges()
            .into_iter()
            .map(|(node_id, start, end)| (node_id, (start, end)))
            .collect();

        let root_element_id = dom.try_root_element().map(|el| el.id);

        Self {
            dom,
            scale,
            width,
            height,
            initial_x,
            initial_y,
            root_element_id,
            #[cfg(feature = "scrollbars")]
            hovered_scrollbar: dom.hovered_scrollbar(),
            #[cfg(feature = "scrollbars")]
            scrollbar_drag_target: dom.scrollbar_drag_target(),
            draw_text_context: RefCell::new(DrawTextContext::default()),
            selection_ranges,
            cache: None,
            recording: RefCell::new(Vec::new()),
            canvas_background: Self::canvas_background_node(dom),
            scrollport_cull: true,
            canvas: true,
            pieces: RefCell::new(Vec::new()),
            pinned: HashSet::new(),
            pinned_pieces: RefCell::new(Vec::new()),
        }
    }

    /// Draw the current tree to current render surface
    /// Eventually we'll want the surface itself to be passed into the render function, along with things like the viewport
    ///
    /// This assumes styles are resolved and layout is complete.
    /// Make sure you do those before trying to render
    pub fn paint_scene(&self, scene: &mut Scene) {
        if self.dom.has_pending_critical_resources() {
            return;
        }

        // Simply render the document (the root element (note that this is not the same as the root node)))
        // scene.reset();
        let viewport_scroll = self.dom.as_ref().viewport_scroll();

        // A document without a root element (e.g. an empty iframe sub-document) has
        // nothing to paint.
        let Some(root_element) = self.dom.as_ref().try_root_element() else {
            return;
        };
        let root_id = root_element.id;
        // The root clip rectangle is the viewport (in screen coordinates, with the
        // initial offset already subtracted). Elements outside of this are culled, and
        // scrollports narrow this rectangle further for their descendants.
        let viewport_clip_rect = self.viewport();
        let root_transform = Affine::translate(Vec2 {
            x: self.initial_x - (viewport_scroll.x * self.scale),
            y: self.initial_y - (viewport_scroll.y * self.scale),
        });
        if self.canvas {
            self.draw_canvas_background(scene, root_transform);
        }

        // acrender: cached painting flattens the root fragment instead
        match self.cache {
            Some(cache) => {
                // the canvas background's element doesn't paint its own background: a fragment
                // recorded for another one is wrong
                let key = format!(
                    "{} {:?} {:?} {} {:?}",
                    self.scale,
                    self.hovered_scrollbar_key(),
                    self.drag_target_key(),
                    self.canvas,
                    self.canvas_background,
                );
                cache.borrow_mut().begin(key);
                let root = &self.dom.as_ref().tree()[root_id];
                let place = PaintCache::place(root, root_transform, self.scale);
                self.ensure(cache, root_id, place);
                let mut runs = Vec::new();
                let mut pieces = self.pieces.borrow_mut();
                let mut start = scene.commands.len();
                let mut deferred = Vec::new();
                Self::flatten(
                    &cache.borrow(),
                    self.dom,
                    (&mut pieces, &mut start),
                    (&mut runs, (&self.pinned, &mut deferred)),
                    root_id,
                    place,
                    self.scale,
                );
                let mut pinned = self.pinned_pieces.borrow_mut();
                while let Some((node, place)) = deferred.pop() {
                    let (mut own, mut at) = (Vec::new(), 0);
                    Self::flatten(
                        &cache.borrow(),
                        self.dom,
                        (&mut own, &mut at),
                        (&mut Vec::new(), (&self.pinned, &mut deferred)),
                        node,
                        place,
                        self.scale,
                    );
                    pinned.push((node, own));
                }
                let mut cache = cache.borrow_mut();
                for (run, place) in runs {
                    cache.ran(run, place);
                }
                cache.end(self.dom);
            }
            None => self.render_element(scene, root_id, root_transform, viewport_clip_rect),
        }
        // acrender: no devtools overlays (layout highlight, hovered / selected node)
    }

    /// The viewport in screen coordinates (the initial offset already subtracted)
    fn viewport(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f64, self.height as f64)
    }

    #[cfg(feature = "scrollbars")]
    fn hovered_scrollbar_key(&self) -> Option<blitz_dom::node::ScrollbarRef> {
        self.hovered_scrollbar
    }
    #[cfg(not(feature = "scrollbars"))]
    fn hovered_scrollbar_key(&self) -> Option<()> {
        None
    }
    #[cfg(feature = "scrollbars")]
    fn drag_target_key(&self) -> Option<blitz_dom::node::ScrollbarRef> {
        self.scrollbar_drag_target
    }
    #[cfg(not(feature = "scrollbars"))]
    fn drag_target_key(&self) -> Option<()> {
        None
    }

    /// acrender: the element whose background is the canvas's (CSS Backgrounds 3 §2.11.2): the root
    /// element when it has a background (a color or an image layer), else its <body>.
    fn canvas_background_node(dom: &BaseDocument) -> Option<NodeId> {
        let has_background = |node: &Node| {
            node.primary_styles().is_some_and(|s| {
                let bg = s.get_background();
                let color = s
                    .clone_background_color()
                    .resolve_to_absolute(&s.clone_color());
                color.alpha > 0.0
                    || bg
                        .background_image
                        .0
                        .iter()
                        .any(|i| !matches!(i, style::values::computed::image::Image::None))
            })
        };
        let root = dom.try_root_element()?;
        if has_background(root) {
            return Some(root.id);
        }
        root.children
            .iter()
            .filter_map(|id| dom.get_node(*id))
            .find(|n| n.data.is_element_with_tag_name(&local_name!("body")))
            .filter(|body| has_background(body))
            .map(|body| body.id)
    }

    /// acrender: paints the canvas background: the whole background (color and image layers) of the
    /// root / <body> (see canvas_background_node), over the whole canvas (at least the viewport),
    /// positioned against it. Upstream filled only the background color, so a gradient or image on
    /// html / body only covered that element's own box.
    fn draw_canvas_background(&self, scene: &mut Scene, root_transform: Affine) {
        let Some(node) = self.canvas_background.and_then(|id| self.dom.get_node(id)) else {
            return;
        };
        let root = self.dom.try_root_element().map_or(Size::ZERO, |r| {
            let s = r.final_layout().size;
            Size::new(s.width as f64, s.height as f64)
        });
        // positioned against the root element's box, painting the whole canvas (at least the viewport)
        let Some(root_element) = self.dom.try_root_element() else {
            return;
        };
        let mut layout = *root_element.final_layout();
        layout.location = taffy::Point::ZERO;
        let canvas = Rect::new(
            0.0,
            0.0,
            (self.width as f64).max(root.width * self.scale),
            (self.height as f64).max(root.height * self.scale),
        );
        let mut cx = self.element_cx(node, layout, root_transform);
        cx.canvas = Some(canvas);
        cx.draw_background(scene);
    }

    /// acrender: records `node_id`'s fragment in its local space (border box at the origin, before
    /// its own transform); boundary children inside it become references
    fn record(&self, cache: &RefCell<PaintCache>, node_id: NodeId, placement: Affine) {
        let location = self.dom.as_ref().tree()[node_id].final_layout().location;
        let origin = Vec2::new(location.x as f64, location.y as f64) * self.scale;
        let viewport = self.viewport();
        self.recording
            .borrow_mut()
            .push(Builder::new(node_id, placement));
        let mut scene = Scene::new();
        self.render_element(&mut scene, node_id, Affine::translate(-origin), viewport);
        let builder = self.recording.borrow_mut().pop().expect("recording stack");
        cache.borrow_mut().store(node_id, scene, builder, self.dom);
    }

    /// acrender: makes `node_id`'s fragment current (reused or re-recorded) for `placement` (its
    /// local space → the scene), then those it references
    fn ensure(&self, cache: &RefCell<PaintCache>, node_id: NodeId, placement: Affine) {
        if !cache.borrow_mut().reuse(
            self.dom,
            node_id,
            (&self.selection_ranges, &self.pinned),
            placement,
            self.viewport(),
        ) {
            self.record(cache, node_id, placement);
        }
        // By index, not a copy of the list: ensuring a child stores only the child's fragment
        let at = |i: usize| {
            cache
                .borrow()
                .get(node_id)
                .and_then(|f| f.refs.get(i).copied())
        };
        for r in (0..).map_while(at) {
            if let Some(child) = self.dom.get_node(r.node) {
                self.ensure(
                    cache,
                    r.node,
                    placement * PaintCache::place(child, r.transform, self.scale),
                );
            }
        }
    }

    /// acrender: appends `node_id`'s fragment to `out` with `transform`, its references in place;
    /// `runs` gets each run of commands appended, with the fragment's placement. A pinned reference
    /// is not appended: it goes to `deferred` with its placement.
    #[allow(clippy::type_complexity)]
    fn flatten(
        cache: &PaintCache,
        dom: &BaseDocument,
        out: (&mut Vec<Piece>, &mut usize),
        (runs, (pinned, deferred)): (
            &mut Vec<(Run, Affine)>,
            (&HashSet<NodeId>, &mut Vec<(NodeId, Affine)>),
        ),
        node_id: NodeId,
        transform: Affine,
        scale: f64,
    ) {
        let Some(fragment) = cache.get(node_id) else {
            return;
        };
        let (pieces, at) = out;
        let piece = |pieces: &mut Vec<Piece>,
                     at: &mut usize,
                     runs: &mut Vec<(Run, Affine)>,
                     range: std::ops::Range<usize>| {
            let run = Run {
                start: *at,
                len: range.len(),
                node: node_id,
                local: range.start,
            };
            runs.push((run, transform));
            *at += range.len();
            if !range.is_empty() {
                pieces.push(Piece {
                    commands: fragment.commands.clone(),
                    range,
                    transform,
                });
            }
        };
        let mut start = 0;
        for r in &fragment.refs {
            piece(pieces, at, runs, start..r.at);
            start = r.at;
            if let Some(child) = dom.get_node(r.node) {
                let place = transform * PaintCache::place(child, r.transform, scale);
                match pinned.contains(&r.node) {
                    true => deferred.push((r.node, place)),
                    false => Self::flatten(
                        cache,
                        dom,
                        (pieces, at),
                        (runs, (pinned, deferred)),
                        r.node,
                        place,
                        scale,
                    ),
                }
            }
        }
        piece(pieces, at, runs, start..fragment.commands.len());
    }

    /// acrender: paints just the element `node_id` and its subtree, without the document background
    /// or its ancestors' transforms, opacity and clips: `transform` maps the element's border box
    /// (at the origin, in physical pixels) into `scene`; the element's own transform still applies.
    pub fn paint_element(&self, scene: &mut Scene, node_id: NodeId, transform: Affine) {
        let location = self.dom.as_ref().tree()[node_id].final_layout().location;
        let origin = Vec2::new(location.x as f64, location.y as f64) * self.scale;
        let everywhere = Rect::new(-1e9, -1e9, 1e9, 1e9);
        self.render_element(
            scene,
            node_id,
            transform * Affine::translate(-origin),
            everywhere,
        );
    }

    /// Renders a node, but is guaranteed that the node is an element
    /// This is because the font_size is calculated from layout resolution and all text is rendered directly here, instead
    /// of a separate text stroking phase.
    ///
    /// In Blitz, text styling gets its attributes from its container element/resolved styles
    /// In other libraries, text gets its attributes from a `text` element - this is not how HTML works.
    ///
    /// Approaching rendering this way guarantees we have all the styles we need when rendering text with not having
    /// to traverse back to the parent for its styles, or needing to pass down styles
    fn render_element(
        &self,
        scene: &mut Scene,
        node_id: NodeId,
        parent_style_transform: Affine,
        clip_rect: Rect,
    ) {
        let node = &self.dom.as_ref().tree()[node_id];

        // acrender: while recording a fragment, a boundary child becomes a reference (it gets its own
        // fragment) and every other element is noted as one this fragment depends on
        let recording = self.recording.borrow().last().map(|b| b.node);
        if recording.is_none() && self.pinned.contains(&node_id) {
            return;
        }
        if let Some(root) = recording {
            if root != node_id && (PaintCache::is_boundary(node) || self.pinned.contains(&node_id))
            {
                let r = Ref {
                    at: scene.commands.len(),
                    node: node_id,
                    transform: parent_style_transform,
                };
                self.recording
                    .borrow_mut()
                    .last_mut()
                    .expect("recording")
                    .refs
                    .push(r);
                return;
            }
            let selection = self.selection_ranges.get(&node_id).copied();
            self.recording
                .borrow_mut()
                .last_mut()
                .expect("recording")
                .visit(self.dom, node, selection);
        }

        // Early return if the element is hidden
        if matches!(node.taffy_display(), taffy::Display::None) {
            return;
        }

        // Only draw elements with a style
        let Some(styles) = node.primary_styles() else {
            return;
        };

        // Hide inputs with type=hidden
        // Implemented here rather than using the style engine for performance reasons
        if node.local_name() == "input" && node.attr(local_name!("type")) == Some("hidden") {
            return;
        }

        // Hide elements with a visibility style other than visible
        if styles.get_inherited_box().visibility != StyloVisibility::Visible {
            return;
        }

        let effects = styles.get_effects();
        let opacity = effects.opacity;
        if opacity == 0.0 {
            return;
        }
        let has_opacity = opacity < 1.0;

        // TODO: account for overflow_x vs overflow_y
        let overflow_x = styles.get_box().overflow_x;
        let overflow_y = styles.get_box().overflow_y;
        // `contain: paint` (and stronger values like `strict`/`content`) clips the element's
        // contents to its padding box. Paint containment does not apply to non-atomic inlines
        // or internal table boxes other than table-cell.
        let contain_paint = styles.get_box().clone_contain().contains(Contain::PAINT) && {
            let display = styles.clone_display();
            let is_internal_table_box_other_than_cell = display.outside()
                == DisplayOutside::InternalTable
                && display.inside() != DisplayInside::TableCell;
            !display.is_inline_flow() && !is_internal_table_box_other_than_cell
        };
        let is_image = node
            .element_data()
            .and_then(|e| e.raster_image_data())
            .is_some();
        let is_sub_doc = node
            .element_data()
            .and_then(|el| el.sub_doc_data())
            .is_some();
        let is_text_input = node
            .element_data()
            .and_then(|el| el.text_input_data())
            .is_some();
        // The root element's overflow is propagated to the viewport (which is clipped by the
        // window/surface bounds), so the root element must not clip its own overflow.
        let is_root_element = self.root_element_id == Some(node_id);
        let should_clip = !is_root_element
            && (is_image
                || is_sub_doc
                || is_text_input
                || contain_paint
                || !matches!(overflow_x, Overflow::Visible)
                || !matches!(overflow_y, Overflow::Visible));

        // Apply padding/border offset to inline root
        let taffy::Layout {
            size,
            border,
            padding,
            location,
            ..
        } = *node.final_layout();
        let box_position = Vec2::new(location.x as f64, location.y as f64) * self.scale;
        let box_size = Size::new(size.width as f64, size.height as f64);
        // acrender: in the element's own space, like `overflow` (upstream placed it at
        // `box_position`, unscaled, which `transform` translates by again: the culling and fragment
        // bounds grew by the element's position, so a moved fragment never looked in view)
        let border_box = Rect::from_origin_size(Point::ZERO, box_size * self.scale);
        let scaled_pb = (padding + border).map(f64::from);
        let content_position = kurbo::Point {
            x: scaled_pb.left,
            y: scaled_pb.top,
        };
        let content_box_size = kurbo::Size {
            width: (size.width as f64 - scaled_pb.left - scaled_pb.right) * self.scale,
            height: (size.height as f64 - scaled_pb.top - scaled_pb.bottom) * self.scale,
        };

        // Don't render things that are out of view
        let overflow = *node.scrollable_overflow();
        let transform = parent_style_transform
            * Affine::translate(box_position)
            * node.transform().as_deref().copied().unwrap_or_default();

        // acrender: while recording a fragment `transform` is in the fragment's local space; its
        // placement on screen (where this frame puts it) maps it back for culling
        let placement = self
            .recording
            .borrow()
            .last()
            .map_or(Affine::IDENTITY, |b| b.placement);
        let screen_transform = Affine::translate(Vec2 {
            x: -self.initial_x,
            y: -self.initial_y,
        }) * placement
            * transform;
        let screen_bbox = screen_transform.transform_rect_bbox(overflow.union(border_box));

        // Cull elements that fall entirely outside the current clip rectangle. In addition to
        // the viewport, `clip_rect` is narrowed by any ancestor scrollport (see below), so this
        // also culls elements scrolled out of view inside a clipping/scrolling container.
        let culled = screen_bbox.x1 < clip_rect.x0
            || screen_bbox.x0 > clip_rect.x1
            || screen_bbox.y1 < clip_rect.y0
            || screen_bbox.y0 > clip_rect.y1;
        if let Some(b) = self.recording.borrow_mut().last_mut() {
            b.painted(
                culled.then_some(overflow),
                transform.transform_rect_bbox(overflow.union(border_box)),
            );
        }
        if culled {
            return;
        }

        // Cull elements whose transformed geometry is not representable in f32 (e.g.
        // `transform: scale(99e99)`): the renderer works in f32 and flattening such paths
        // can attempt to allocate an unbounded number of line segments.
        let f32_max = f64::from(f32::MAX);
        if !screen_bbox.is_finite()
            || [
                screen_bbox.x0,
                screen_bbox.y0,
                screen_bbox.x1,
                screen_bbox.y1,
            ]
            .iter()
            .any(|c| c.abs() > f32_max)
        {
            return;
        }

        // Optimise zero-area (/very small area) clips by not rendering at all
        let clip_area = content_box_size.width * content_box_size.height;
        let overflow_area =
            node.scrollable_overflow().width() * node.scrollable_overflow().height();
        if should_clip && clip_area < 0.01 && overflow_area < 0.01 {
            return;
        }

        // Apply CSS transform property (where transforms are 2d)

        let mut cx = self.element_cx(node, *node.final_layout(), transform);

        // If this element clips its overflow it establishes a scrollport: narrow the clip
        // rectangle passed to descendants to the visible (clipped) region so that content
        // scrolled out of view is culled rather than drawn and clipped away. The box used
        // here matches the clip applied to the content below.
        // acrender: cached and reference painting cull against the viewport only (a fragment is reused
        // wherever it lands, not just inside the scrollport it was recorded in)
        let child_clip_rect = if should_clip && self.scrollport_cull {
            // acrender: the box's rect (the bounding box of its path)
            let clip_box = if is_text_input {
                cx.frame.content_box
            } else {
                cx.frame.padding_box
            };
            clip_rect.intersect(screen_transform.transform_rect_bbox(clip_box))
        } else {
            clip_rect
        };

        // Compute clip-path (if any) and wrap all rendering in a clip layer
        let clip_path_for_layer = cx.clip_path_shape().map(|mut path| {
            path.apply_affine(Affine::scale(self.scale));
            path
        });

        // CSS 2 `clip` property (absolutely positioned elements only). Applies to the whole of
        // the element's rendering (including outlines and shadows), so it is the outermost layer.
        let css_clip_rect = cx.css_clip_rect();
        if css_clip_rect.is_some_and(|rect| rect.is_zero_area()) {
            return;
        }
        if let Some(rect) = &css_clip_rect {
            layers::push_layer(scene, rect, 1.0, cx.transform, None, None);
        }

        // acrender: the outline is drawn last (below), over the background and content
        let outline_transform = cx.transform;
        cx.draw_outset_box_shadow(scene);

        // clip-path clip ayer
        layers::maybe_with_layer(
            scene,
            clip_path_for_layer.as_ref(),
            1.0,
            cx.transform,
            None,
            None,
            |scene| {
                // If the element has a CSS `mask`, then push an isolation layer for the
                // masked content. The mask is applied when the layer is popped below.
                let mask_layer_pushed = cx.maybe_push_css_mask_layer(scene);
                // `cx.transform` is mutated to apply scroll offsets while drawing content.
                // Save it so that the mask can be drawn untransformed by scroll offsets.
                let unscrolled_transform = cx.transform;

                let filter = convert_filters(&effects.filter.0).map(Arc::new);
                let backdrop_filter = convert_filters(&effects.backdrop_filter.0).map(Arc::new);

                // Adjust effect layer clip by filter expansion area
                //
                // Returns a rectangle centered at the origin representing how much the filter
                // expands the processing region in each direction. The rect coordinates are:
                // - x0: negative left expansion
                // - y0: negative top expansion
                // - x1: positive right expansion
                // - y1: positive bottom expansion
                let filter_expansion_area =
                    filter.as_ref().map(|f| f.expansion()).unwrap_or(Rect::ZERO);

                // acrender: the border box (the bounding box of its path)
                let mut effect_layer_clip = cx.frame.border_box;
                effect_layer_clip.x0 += filter_expansion_area.x0;
                effect_layer_clip.y0 += filter_expansion_area.y0;
                effect_layer_clip.x1 += filter_expansion_area.x1;
                effect_layer_clip.y1 += filter_expansion_area.y1;

                // Opacity/Filter layer if box has opacity or a filter.
                // Clipped to border-box as it needs to include the background and borders.
                let has_effect = has_opacity || filter.is_some() || backdrop_filter.is_some();
                layers::maybe_with_layer(
                    scene,
                    has_effect.then_some(&effect_layer_clip),
                    opacity,
                    cx.transform,
                    filter,
                    backdrop_filter,
                    |scene| {
                        // acrender: the canvas already has this element's background
                        if Some(node_id) != self.canvas_background {
                            cx.draw_background(scene);
                        }
                        cx.draw_inset_box_shadow(scene);
                        cx.draw_table_row_backgrounds(scene);
                        cx.draw_table_borders(scene);
                        cx.draw_border(scene);

                        // TODO: allow layers with opacity to be unclipped (overflow: visible)
                        let clip = should_clip.then(|| {
                            if is_text_input {
                                cx.frame.content_box_path()
                            } else {
                                cx.frame.padding_box_path()
                            }
                        });

                        // Clip layer if box requires clipping. Opacity set to 1.0
                        layers::maybe_with_layer(
                            scene,
                            clip.as_ref(),
                            1.0, // opacity
                            cx.transform,
                            None,
                            None,
                            |scene| {
                                // Now that background has been drawn, offset pos and cx in order to draw our contents scrolled
                                let content_position = Point {
                                    x: content_position.x - node.scroll_offset().x,
                                    y: content_position.y - node.scroll_offset().y,
                                };

                                // acrender: scroll in the element's space (an ancestor's scale applies)
                                cx.transform = cx.transform.pre_translate(Vec2 {
                                    x: -node.scroll_offset().x * self.scale,
                                    y: -node.scroll_offset().y * self.scale,
                                });
                                cx.draw_image(scene);
                                cx.draw_canvas_picture(scene);
                                #[cfg(feature = "svg")]
                                cx.draw_svg(scene);
                                cx.draw_sub_document(scene);
                                cx.draw_input(scene);
                                cx.draw_select(scene);
                                cx.draw_text_input_text(scene, content_position);
                                cx.draw_inline_layout(scene, content_position);
                                cx.draw_marker(scene, content_position);
                                cx.draw_children(scene, cx.transform, child_clip_rect);
                            },
                        );

                        // Overlay scrollbars, drawn unscrolled above the
                        // clipped content.
                        #[cfg(feature = "scrollbars")]
                        {
                            cx.transform = unscrolled_transform;
                            cx.draw_scrollbars(scene);
                        }
                    },
                );

                // Apply the CSS `mask` (if any) to the content drawn above
                cx.transform = unscrolled_transform;
                cx.maybe_pop_css_mask_layer(scene, mask_layer_pushed);
            },
        );

        cx.transform = outline_transform;
        cx.draw_outline(scene);

        if css_clip_rect.is_some() {
            scene.pop_layer();
        }
    }

    fn render_node(
        &self,
        scene: &mut Scene,
        node_id: NodeId,
        parent_style_transform: Affine,
        clip_rect: Rect,
    ) {
        let node = &self.dom.as_ref().tree()[node_id];

        match &node.data {
            NodeData::Element(_) | NodeData::AnonymousBlock(_) => {
                self.render_element(scene, node_id, parent_style_transform, clip_rect)
            }
            NodeData::Text(TextNodeData { .. }) => {
                // Text nodes should never be rendered directly
                // (they should always be rendered as part of an inline layout)
                // unreachable!()
            }
            NodeData::Document(_) => {}
            // NodeData::Doctype => {}
            NodeData::Comment { .. } => {} // NodeData::ProcessingInstruction { .. } => {}
        }
    }

    /// acrender: one line's piece of an inline element with edges (see blitz-dom `InlineEdge` and
    /// text.rs): its background and border, drawn like a block's with `frame` as the box (sides and
    /// corners cut where the element wraps already left out of it)
    pub(crate) fn draw_inline_fragment(
        &'dom self,
        scene: &mut impl PaintScene,
        node: &'dom Node,
        frame: CssBox,
        transform: Affine,
    ) {
        let mut cx = self.element_cx(node, Layout::new(), transform);
        cx.frame = frame;
        cx.draw_background(scene);
        cx.draw_border(scene);
    }

    fn element_cx(
        &'dom self,
        node: &'dom Node,
        layout: Layout,
        transform: Affine,
    ) -> ElementCx<'dom, 'a> {
        let style = node
            .stylo_element_data()
            .primary_styles()
            .as_ref()
            .map(|styles| (*styles).clone())
            .unwrap_or_else(|| {
                ComputedValues::initial_values_with_font_override(Font::initial_values()).to_arc()
            });

        let scale = self.scale;

        // todo: maybe cache this so we don't need to constantly be figuring it out
        // It is quite a bit of math to calculate during render/traverse
        // Also! we can cache the bezpaths themselves, saving us a bunch of work
        // acrender: a fieldset with a rendered legend paints its box from the border through the
        // legend's middle, with a gap in the border behind the legend
        let (frame, legend_gap) = match self.dom.rendered_legend(node.id) {
            Some(legend) => fieldset_frame(&style, &layout, self.dom.get_node(legend), scale),
            None => (create_css_rect(&style, &layout, scale), None),
        };

        let element = node.element_data().unwrap();

        ElementCx {
            context: self,
            frame,
            legend_gap,
            scale,
            style,
            node,
            element,
            transform,
            #[cfg(feature = "svg")]
            svg: match element.image_data() {
                Some(blitz_dom::node::ImageData::Svg(svg)) => Some(svg),
                _ => None,
            },
            #[cfg(feature = "svg")]
            svg_simple: element.svg_simple(),
            text_input: element.text_input_data(),
            list_item: element.list_item_data.as_deref(),
            canvas: None,
        }
    }
}

fn to_image_quality(image_rendering: ImageRendering) -> peniko::ImageQuality {
    match image_rendering {
        ImageRendering::Auto => peniko::ImageQuality::Medium,
        ImageRendering::CrispEdges => peniko::ImageQuality::Low,
        ImageRendering::Pixelated => peniko::ImageQuality::Low,
    }
}

/// The transform from an SVG's rendered tree to a viewport of the given size.
#[cfg(feature = "svg")]
fn svg_viewport_affine(svg: &blitz_dom::node::SvgImageData, width: f32, height: f32) -> Affine {
    let t = svg.viewport_transform(width, height);
    Affine::new([t.sx, t.ky, t.kx, t.sy, t.tx, t.ty].map(f64::from))
}

/// Ensure that the `resized_image` field has a correctly sized image
fn to_peniko_image(image: &RasterImageData, quality: peniko::ImageQuality) -> peniko::ImageBrush {
    peniko::ImageBrush {
        image: ImageData {
            data: image.data.clone(),
            format: peniko::ImageFormat::Rgba8,
            width: image.width,
            height: image.height,
            alpha_type: peniko::ImageAlphaType::Alpha,
        },
        sampler: ImageSampler {
            x_extend: peniko::Extend::Repeat,
            y_extend: peniko::Extend::Repeat,
            quality,
            alpha: 1.0,
        },
    }
}

/// A context of loaded and hot data to draw the element from
struct ElementCx<'dom, 'a> {
    context: &'dom BlitzDomPainter<'dom, 'a>,
    frame: CssBox,
    /// acrender: a fieldset's border is not painted behind its rendered legend (this rectangle)
    legend_gap: Option<Rect>,
    style: style::servo_arc::Arc<ComputedValues>,
    scale: f64,
    node: &'dom Node,
    element: &'dom ElementData,
    transform: Affine,
    #[cfg(feature = "svg")]
    svg: Option<&'dom blitz_dom::node::SvgImageData>,
    #[cfg(feature = "svg")]
    svg_simple: Option<&'dom blitz_dom::node::SimpleSvg>,
    text_input: Option<&'dom TextInputData>,
    list_item: Option<&'dom ListItemLayout>,
    /// acrender: set when painting the canvas background: the whole canvas, which background layers
    /// clip to and tile over (they are still positioned against `frame`, the root element's box)
    canvas: Option<Rect>,
}

/// Converts parley BoundingBox into peniko Rect
fn convert_rect(rect: &parley::BoundingBox) -> kurbo::Rect {
    peniko::kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1)
}

impl ElementCx<'_, '_> {
    /// Paint overlay scrollbar thumbs for scroll containers: `overflow:
    /// scroll`, or `auto` when the content overflows (never `hidden`/`clip`,
    /// which scroll only programmatically). Thumbs appear on scroll and fade
    /// out after a delay ([`BaseDocument::scrollbar_opacity`]); never-scrolled
    /// containers paint nothing, keeping thumbs out of static reftest
    /// screenshots.
    ///
    /// Geometry comes from [`Node::scrollbar_thumb`], shared with the
    /// thumb-drag hit testing in blitz-dom.
    #[cfg(feature = "scrollbars")]
    fn draw_scrollbars(&self, scene: &mut impl PaintScene) {
        // css-scrollbars-1 scrollbar-color: author thumb/track colors
        use blitz_dom::node::{ScrollbarColor, ScrollbarRef};
        use taffy::AbsoluteAxis;
        let (custom_thumb, custom_track) = match self.node.scrollbar_color() {
            ScrollbarColor::Auto => (None, None),
            ScrollbarColor::Colors { thumb, track } => {
                (Some(thumb.as_srgb_color()), Some(track.as_srgb_color()))
            }
        };

        let drag_target = self.context.scrollbar_drag_target;
        let hovered_thumb = self.context.hovered_scrollbar;

        // scrollbar-color doesn't affect overlay visibility: persistence is
        // UA policy, not author styling.
        let node_id = self.node.id;
        let opacity = self.context.dom.scrollbar_opacity(node_id);
        if opacity == 0.0 {
            return;
        }

        // Default thumb palette for the used color scheme; thumbs paint as
        // fill plus a thin contrast stroke so they read over same-colored
        // content.
        let dark_scheme =
            self.context.dom.viewport().color_scheme == blitz_traits::shell::ColorScheme::Dark;
        let (thumb_rest, thumb_hover, thumb_active, stroke_color) = if dark_scheme {
            (
                Color::from_rgba8(214, 214, 214, 178),
                Color::from_rgba8(190, 190, 190, 222),
                Color::from_rgba8(172, 172, 172, 255),
                Color::from_rgba8(0, 0, 0, 102),
            )
        } else {
            (
                Color::from_rgba8(128, 128, 128, 178),
                Color::from_rgba8(152, 152, 152, 222),
                Color::from_rgba8(170, 170, 170, 255),
                Color::from_rgba8(255, 255, 255, 102),
            )
        };

        // Chromium's hovered/pressed scrollbar contrast ratios.
        const HOVER_CONTRAST: f32 = 1.8;
        const ACTIVE_CONTRAST: f32 = 1.3;

        for axis in [AbsoluteAxis::Vertical, AbsoluteAxis::Horizontal] {
            if !self.node.wants_scrollbar(axis) {
                continue;
            }
            let Some(thumb) = self.node.scrollbar_thumb(axis) else {
                continue;
            };

            let rect = thumb.scale_from_origin(self.scale);

            // Track (only when the author specified a track color)
            if let Some(track_color) = custom_track {
                let padding_box = self.frame.padding_box;
                let track_rect = match axis {
                    AbsoluteAxis::Horizontal => {
                        Rect::new(padding_box.x0, rect.y0, padding_box.x1, rect.y1)
                    }
                    AbsoluteAxis::Vertical => {
                        Rect::new(rect.x0, padding_box.y0, rect.x1, padding_box.y1)
                    }
                };
                scene.fill(
                    Fill::NonZero,
                    self.transform,
                    track_color.multiply_alpha(opacity),
                    None,
                    &track_rect,
                );
            }

            let this = ScrollbarRef { node_id, axis };
            let is_active = drag_target == Some(this);
            let is_hovered = hovered_thumb == Some(this);
            let color = match custom_thumb {
                Some(base) if is_active => crate::color::blend_for_contrast(base, ACTIVE_CONTRAST),
                Some(base) if is_hovered => crate::color::blend_for_contrast(base, HOVER_CONTRAST),
                Some(base) => base,
                None if is_active => thumb_active,
                None if is_hovered => thumb_hover,
                None => thumb_rest,
            };
            let radius = match axis {
                AbsoluteAxis::Horizontal => rect.height() / 2.0,
                AbsoluteAxis::Vertical => rect.width() / 2.0,
            };
            scene.fill(
                Fill::NonZero,
                self.transform,
                color.multiply_alpha(opacity),
                None,
                &rect.to_rounded_rect(radius),
            );
            // Contrast stroke, default thumbs only: an author-specified
            // scrollbar-color is rendered exactly as given.
            if custom_thumb.is_none() {
                let stroke_width = self.scale;
                let stroke_rect = rect.inset(-stroke_width / 2.0);
                scene.stroke(
                    &Stroke::new(stroke_width),
                    self.transform,
                    stroke_color.multiply_alpha(opacity),
                    None,
                    &stroke_rect.to_rounded_rect(radius - stroke_width / 2.0),
                );
            }
        }
    }

    /// acrender: "…" for `text-overflow: ellipsis` (when the box clips its overflow) and
    /// `-webkit-line-clamp`, with the marker shaped by layout.
    fn ellipsis<'l>(&self, text_layout: &'l TextLayout) -> Option<crate::text::Ellipsis<'l>> {
        use style::values::specified::text::TextOverflowSide;
        let marker = text_layout.ellipsis.as_ref()?;
        let clips = !matches!(self.style.get_box().overflow_x, Overflow::Visible);
        Some(crate::text::Ellipsis {
            end: self.frame.content_box.width() as f32,
            marker,
            overflow: clips
                && self.style.get_text().text_overflow.second == TextOverflowSide::Ellipsis,
            clamp_line: text_layout.clamp.map(|n| n - 1),
            align: {
                use style::computed_values::direction::T as Direction;
                use style::values::specified::TextAlignKeyword as Align;
                let rtl = self.style.get_inherited_box().direction == Direction::Rtl;
                match self.style.clone_text_align() {
                    Align::Center | Align::MozCenter => 0.5,
                    Align::Right | Align::MozRight => 1.0,
                    Align::End => (!rtl).into(),
                    Align::Start => rtl.into(),
                    Align::Left | Align::MozLeft | Align::Justify => 0.0,
                }
            },
        })
    }

    fn draw_inline_layout(&self, scene: &mut impl PaintScene, pos: Point) {
        if self.node.flags.is_inline_root() {
            let text_layout = self.element
                .inline_layout_data
                .as_ref()
                .unwrap_or_else(|| {
                    panic!("Tried to render node marked as inline root that does not have an inline layout: {:?}", self.node);
                });

            let pos = Point {
                x: pos.x,
                y: pos.y + text_layout.block_offset as f64,
            };
            let transform =
                self.transform * Affine::translate((pos.x * self.scale, pos.y * self.scale));

            // Render inline element backgrounds (e.g. `<span style="background: ...">`)
            // behind the text and selection highlight.
            crate::text::draw_inline_backgrounds(
                scene,
                text_layout.layout.lines(),
                self.context,
                transform,
                self.node.id,
            );

            // Render text selection highlight (if any) using cached selection ranges
            if let Some(&(sel_start, sel_end)) = self.context.selection_ranges.get(&self.node.id) {
                crate::text::draw_text_selection(
                    scene,
                    &text_layout.layout,
                    transform,
                    sel_start,
                    sel_end,
                );
            }

            // Render text
            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            crate::text::stroke_text(
                scene,
                text_layout.layout.lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                self.ellipsis(text_layout),
                &mut draw_text_context,
            );
        }
    }

    fn draw_text_input_text(&self, scene: &mut impl PaintScene, pos: Point) {
        // Render the text in text inputs
        if let Some(input_data) = self.text_input {
            // For single-line inputs, add an offset to vertically center the text input layout
            // within the content box of it's node.
            let y_offset = self.node.text_input_v_centering_offset(self.scale);
            let pos = Point {
                x: pos.x,
                y: pos.y + y_offset,
            };

            // Apply the scroll offset (stored in CSS pixels, scaled here to device pixels) so
            // that the caret stays visible. Single-line inputs scroll horizontally; multi-line
            // inputs scroll vertically.
            let scroll_offset = input_data.scroll_offset as f64 * self.scale;
            let (scroll_x, scroll_y) = if input_data.is_multiline {
                (0.0, scroll_offset)
            } else {
                (scroll_offset, 0.0)
            };

            let transform = self.transform
                * Affine::translate((pos.x * self.scale - scroll_x, pos.y * self.scale - scroll_y));

            if self.node.is_focussed() {
                // Render selection/caret
                for (rect, _line_idx) in input_data.editor.selection_geometry().iter() {
                    scene.fill(
                        Fill::NonZero,
                        transform,
                        SELECTION_COLOR,
                        None,
                        &convert_rect(rect),
                    );
                }
                if let Some(cursor) = input_data.editor.cursor_geometry(1.5) {
                    let color = self.style.get_inherited_text().color;
                    let caret_color = match &self.style.get_inherited_ui().caret_color.0 {
                        ColorOrAuto::Auto => color,
                        ColorOrAuto::Color(caret_color) => caret_color.resolve_to_absolute(&color),
                    };

                    scene.fill(
                        Fill::NonZero,
                        transform,
                        caret_color.as_srgb_color(),
                        None,
                        &convert_rect(&cursor),
                    );
                };
            }

            // Render text
            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            // acrender: an empty input shows its placeholder, coloured by `::placeholder` (colour and opacity)
            let placeholder = input_data
                .placeholder
                .as_ref()
                .filter(|_| input_data.editor.raw_text().is_empty())
                .and_then(|p| p.try_layout());
            if let Some(layout) = placeholder {
                let style = self.context.dom.placeholder_style(self.node.id);
                let color = style
                    .as_ref()
                    .map_or(self.style.clone_color(), |s| s.clone_color());
                let opacity = style.as_ref().map_or(1.0, |s| s.get_effects().opacity);
                draw_text_context.color = Some(color.as_srgb_color().multiply_alpha(opacity));
                crate::text::stroke_text(
                    scene,
                    layout.lines(),
                    self.context.dom,
                    transform,
                    self.scale,
                    self.node.id,
                    None,
                    &mut draw_text_context,
                );
                return;
            }
            crate::text::stroke_text(
                scene,
                input_data.editor.try_layout().unwrap().lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                None,
                &mut draw_text_context,
            );
        }
    }

    fn draw_marker(&self, scene: &mut impl PaintScene, pos: Point) {
        if let Some(ListItemLayout {
            marker,
            position: ListItemLayoutPosition::Outside(layout),
        }) = self.list_item
        {
            // Right align and pad the bullet when rendering outside
            let x_padding = match marker {
                Marker::Char(_) => 8.0,
                Marker::String(_) => 0.0,
            };
            // Outside markers are placed outside the list item's border box
            // (`pos` is the origin of its content box)
            let item_layout = self.node.final_layout();
            let x_offset = -(layout.full_width() / layout.scale()
                + x_padding
                + item_layout.padding.left
                + item_layout.border.left);

            // Align the marker with the baseline of the first line of text in the list item
            let y_offset = if let Some((text_layout, first_text_line)) = &self
                .element
                .inline_layout_data
                .as_ref()
                .and_then(|text_layout| Some((text_layout, text_layout.layout.lines().next()?)))
            {
                (first_text_line.metrics().baseline
                    - layout.lines().next().unwrap().metrics().baseline)
                    / layout.scale()
                    + text_layout.block_offset
            } else {
                0.0
            };

            let pos = Point {
                x: pos.x + x_offset as f64,
                y: pos.y + y_offset as f64,
            };

            let transform =
                self.transform * Affine::translate((pos.x * self.scale, pos.y * self.scale));

            let mut draw_text_context = self.context.draw_text_context.borrow_mut();
            crate::text::stroke_text(
                scene,
                layout.lines(),
                self.context.dom,
                transform,
                self.scale,
                self.node.id,
                None,
                &mut draw_text_context,
            );
        }
    }

    fn draw_children(&self, scene: &mut Scene, parent_style_transform: Affine, clip_rect: Rect) {
        // Negative z_index hoisted nodes

        if let Some(hoisted) = &self.node.stacking_context {
            for hoisted_child in hoisted.neg_z_hoisted_children() {
                let position = hoisted_child.position(self.node.tree(), self.node.id);
                let pos = kurbo::Vec2 {
                    x: position.x as f64 * self.scale,
                    y: position.y as f64 * self.scale,
                };
                self.render_node(
                    scene,
                    hoisted_child.node_id,
                    parent_style_transform.pre_translate(pos),
                    clip_rect,
                );
            }
        }

        // Regular children
        if let Some(children) = &*self.node.paint_children.borrow() {
            for child_id in children {
                // Fixed-position children of the root element are positioned against
                // the viewport and do not scroll with it, so cancel out the viewport
                // scroll (applied in `paint_scene`). A fixed box contained by a
                // transformed ancestor scrolls with it like any out-of-flow box.
                let child = &self.context.dom.as_ref().tree()[*child_id];
                let child_transform = if child.taffy_position() == taffy::Position::Fixed
                    && Some(self.node.id) == self.context.root_element_id
                {
                    let scroll = self.context.dom.as_ref().viewport_scroll();
                    parent_style_transform.pre_translate(kurbo::Vec2 {
                        x: scroll.x * self.scale,
                        y: scroll.y * self.scale,
                    })
                } else {
                    parent_style_transform
                };
                self.render_node(scene, *child_id, child_transform, clip_rect);
            }
        }

        // Positive z_index hoisted nodes
        if let Some(hoisted) = &self.node.stacking_context {
            for hoisted_child in hoisted.pos_z_hoisted_children() {
                let position = hoisted_child.position(self.node.tree(), self.node.id);
                let pos = kurbo::Vec2 {
                    x: position.x as f64 * self.scale,
                    y: position.y as f64 * self.scale,
                };
                self.render_node(
                    scene,
                    hoisted_child.node_id,
                    parent_style_transform.pre_translate(pos),
                    clip_rect,
                );
            }
        }
    }

    #[cfg(feature = "svg")]
    fn draw_svg(&self, scene: &mut impl PaintScene) {
        use style::properties::generated::longhands::object_fit::computed_value::T as ObjectFit;

        let Some(svg) = self.svg else {
            return;
        };

        let width = self.frame.content_box.width() as f32;
        let height = self.frame.content_box.height() as f32;
        let svg_size = svg.tree.size();

        let x = self.frame.content_box.origin().x;
        let y = self.frame.content_box.origin().y;

        // An SVG with an aspect ratio but no natural size is sized by the default sizing
        // algorithm, which for `none` and `scale-down` contains it within the content box.
        let object_fit = match self.style.clone_object_fit() {
            ObjectFit::None | ObjectFit::ScaleDown
                if svg.intrinsic_width().is_none()
                    && svg.intrinsic_height().is_none()
                    && svg.viewbox_aspect_ratio().is_some() =>
            {
                ObjectFit::Contain
            }
            object_fit => object_fit,
        };
        let object_position = self.style.clone_object_position();

        // Apply object-fit algorithm
        let container_size = taffy::Size { width, height };
        let object_size = taffy::Size {
            width: svg_size.width(),
            height: svg_size.height(),
        };
        let paint_size = compute_object_fit(container_size, Some(object_size), object_fit);

        // Compute object-position
        let x_offset = object_position.horizontal.resolve(
            CSSPixelLength::new(container_size.width - paint_size.width) / self.scale as f32,
        ) * self.scale as f32;
        let y_offset = object_position.vertical.resolve(
            CSSPixelLength::new(container_size.height - paint_size.height) / self.scale as f32,
        ) * self.scale as f32;
        let x = x + x_offset.px() as f64;
        let y = y + y_offset.px() as f64;

        let viewport = svg_viewport_affine(svg, paint_size.width, paint_size.height);
        // acrender: content box offset in the element's space (upstream translates in screen space,
        // ignoring an ancestor's scale / rotation)
        let inner = Affine::translate(Vec2 { x, y }) * viewport;

        self.with_replaced_content_clip(
            scene,
            container_size,
            paint_size,
            x_offset.px(),
            y_offset.px(),
            |scene| {
                // acrender: simple SVGs carry their own drawing commands (no usvg tree)
                match self.svg_simple {
                    Some(simple) => draw_simple_svg(scene, simple, self.transform * inner),
                    None => draw_svg_tree(scene, &svg.tree, self.transform, inner),
                }
            },
        );
    }

    /// Clip replaced content to the content box when its object-fit/object-position
    /// rect overflows it.
    fn with_replaced_content_clip<S: PaintScene>(
        &self,
        scene: &mut S,
        container_size: taffy::Size<f32>,
        paint_size: taffy::Size<f32>,
        x_offset: f32,
        y_offset: f32,
        paint: impl FnOnce(&mut S),
    ) {
        let overflows = x_offset < 0.0
            || y_offset < 0.0
            || x_offset + paint_size.width > container_size.width
            || y_offset + paint_size.height > container_size.height;
        layers::maybe_with_layer(
            scene,
            overflows.then(|| self.frame.content_box_path()).as_ref(),
            1.0,
            self.transform,
            None,
            None,
            paint,
        );
    }

    fn draw_image(&self, scene: &mut impl PaintScene) {
        if let Some(image) = self.element.raster_image_data() {
            let width = self.frame.content_box.width() as u32;
            let height = self.frame.content_box.height() as u32;
            let x = self.frame.content_box.origin().x;
            let y = self.frame.content_box.origin().y;

            let object_fit = self.style.clone_object_fit();
            let object_position = self.style.clone_object_position();
            let image_rendering = self.style.clone_image_rendering();
            let quality = to_image_quality(image_rendering);

            // Apply object-fit algorithm
            let container_size = taffy::Size {
                width: width as f32,
                height: height as f32,
            };
            let object_size = taffy::Size {
                width: image.width as f32,
                height: image.height as f32,
            };
            let paint_size = compute_object_fit(container_size, Some(object_size), object_fit);

            // Compute object-position
            let x_offset = object_position.horizontal.resolve(
                CSSPixelLength::new(container_size.width - paint_size.width) / self.scale as f32,
            ) * self.scale as f32;
            let y_offset = object_position.vertical.resolve(
                CSSPixelLength::new(container_size.height - paint_size.height) / self.scale as f32,
            ) * self.scale as f32;
            let x = x + x_offset.px() as f64;
            let y = y + y_offset.px() as f64;

            let x_scale = paint_size.width as f64 / object_size.width as f64;
            let y_scale = paint_size.height as f64 / object_size.height as f64;
            let transform = self
                .transform
                .pre_translate(Vec2 { x, y })
                .pre_scale_non_uniform(x_scale, y_scale);

            self.with_replaced_content_clip(
                scene,
                container_size,
                paint_size,
                x_offset.px(),
                y_offset.px(),
                |scene| {
                    scene.draw_image(to_peniko_image(image, quality).as_ref(), transform);
                },
            );
        }
    }

    /// acrender: a 2D canvas: its picture (in bitmap pixels, `width` x `height` attributes) fitted into the
    /// content box like an image, in a layer of its own (clearRect / destination-* only touch the canvas)
    fn draw_canvas_picture(&self, scene: &mut impl PaintScene) {
        let video = self.node.local_name() == "video";
        if self.node.local_name() != "canvas" && !video {
            return;
        }
        let Some(picture) = self.context.dom.canvas_picture(self.node.id) else {
            return;
        };
        let attr = |name, default: f32| {
            self.node
                .attr(name)
                .and_then(|v| v.trim().parse::<u32>().ok())
                .map_or(default, |v| v as f32)
        };
        // acrender: a <video>'s picture is its current frame, in the frame's pixels
        let object_size = match self.context.dom.video_size(self.node.id) {
            Some((width, height)) if video => taffy::Size { width, height },
            _ if video => return,
            _ => taffy::Size {
                width: attr(local_name!("width"), 300.0),
                height: attr(local_name!("height"), 150.0),
            },
        };
        if object_size.width <= 0.0 || object_size.height <= 0.0 {
            return;
        }
        let container_size = taffy::Size {
            width: self.frame.content_box.width() as f32,
            height: self.frame.content_box.height() as f32,
        };
        let paint_size = compute_object_fit(
            container_size,
            Some(object_size),
            self.style.clone_object_fit(),
        );
        let object_position = self.style.clone_object_position();
        let x_offset = object_position.horizontal.resolve(
            CSSPixelLength::new(container_size.width - paint_size.width) / self.scale as f32,
        ) * self.scale as f32;
        let y_offset = object_position.vertical.resolve(
            CSSPixelLength::new(container_size.height - paint_size.height) / self.scale as f32,
        ) * self.scale as f32;
        let x = self.frame.content_box.origin().x + x_offset.px() as f64;
        let y = self.frame.content_box.origin().y + y_offset.px() as f64;
        let transform = self
            .transform
            .pre_translate(Vec2 { x, y })
            .pre_scale_non_uniform(
                paint_size.width as f64 / object_size.width as f64,
                paint_size.height as f64 / object_size.height as f64,
            );
        let bitmap = Rect::new(
            0.0,
            0.0,
            object_size.width.into(),
            object_size.height.into(),
        );
        scene.push_layer(
            Fill::NonZero,
            peniko::Mix::Normal,
            1.0,
            transform,
            &bitmap,
            None,
            None,
        );
        scene.append_scene((**picture).clone(), transform);
        scene.pop_layer();
    }

    fn draw_sub_document(&self, scene: &mut Scene) {
        if let Some(sub_doc) = self.element.sub_doc_data().map(|doc| doc.inner()) {
            let scale = self.scale;
            let width = self.frame.content_box.width() as u32;
            let height = self.frame.content_box.height() as u32;

            // TODO: Support arbitrary transforms of subdocuments
            let translation = self.transform.translation();
            let initial_x = translation.x + self.frame.content_box.origin().x;
            let initial_y = translation.y + self.frame.content_box.origin().y;
            // let transform = self.transform.then_translate(Vec2 { x, y });

            let painter =
                BlitzDomPainter::new(&sub_doc, scale, width, height, initial_x, initial_y);
            painter.paint_scene(scene);
        }
    }
}
impl<'dom, 'a> std::ops::Deref for ElementCx<'dom, 'a> {
    type Target = BlitzDomPainter<'dom, 'a>;
    fn deref(&self) -> &Self::Target {
        self.context
    }
}

fn insets_from_taffy_rect(input: taffy::Rect<f64>) -> Insets {
    Insets {
        x0: input.left,
        y0: input.top,
        x1: input.right,
        y1: input.bottom,
    }
}

/// Convert Stylo and Taffy types into Kurbo types
fn create_css_rect(style: &ComputedValues, layout: &Layout, scale: f64) -> CssBox {
    let size = layout.size.map(|v| v as f64);
    let border_box = Rect::new(0.0, 0.0, size.width, size.height);
    create_css_box(style, border_box, layout.border, layout.padding, scale)
}

/// acrender: the box a fieldset with a rendered legend paints, and the gap in its border
/// (HTML's rendering of fieldset): the legend is centred on the block-start border (laid out by
/// blitz-dom's `layout::fieldset`, which gives the border the legend's margin box when it is taller),
/// so the border box starts at the border's top edge there; the content box stays where layout put it.
fn fieldset_frame(
    style: &ComputedValues,
    layout: &Layout,
    legend: Option<&Node>,
    scale: f64,
) -> (CssBox, Option<Rect>) {
    let Some(legend) = legend else {
        return (create_css_rect(style, layout, scale), None);
    };
    let l = legend.final_layout();
    let border = layout.border.top as f64;
    let margin_box = (l.margin.top + l.size.height + l.margin.bottom) as f64;
    let band = border.max(margin_box);
    let centre = match margin_box >= border {
        true => l.margin.top as f64 + l.size.height as f64 / 2.0,
        false => border / 2.0,
    };
    let top = (centre - border / 2.0).max(0.0);
    let size = layout.size.map(|v| v as f64);
    let border_box = Rect::new(0.0, top, size.width, size.height.max(top));
    let mut padding = layout.padding;
    padding.top = ((band + layout.padding.top as f64 - top - border).max(0.0)) as f32;
    let frame = create_css_box(style, border_box, layout.border, padding, scale);
    let (x, y) = (l.location.x as f64, l.location.y as f64);
    let gap = Rect::new(
        x,
        (y - l.margin.top as f64).min(top),
        x + l.size.width as f64,
        (y + l.size.height as f64 + l.margin.bottom as f64).max(top + border),
    );
    (frame, Some(gap.scale_from_origin(scale)))
}

fn create_css_box(
    style: &ComputedValues,
    border_box: Rect,
    border: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    scale: f64,
) -> CssBox {
    // Resolve and rescale
    // We have to scale since document pixels are not same same as rendered pixels
    let width: f64 = border_box.width();
    let height: f64 = border_box.height();
    let border_box = border_box.scale_from_origin(scale);
    let border = insets_from_taffy_rect(border.map(|p| p as f64 * scale));
    let padding = insets_from_taffy_rect(padding.map(|p| p as f64 * scale));
    let outline_width = style.get_outline().outline_width.0.to_f64_px() * scale;

    // Resolve the radii to a length. need to downscale since the radii are in document pixels
    let resolve_w = CSSPixelLength::new(width as _);
    let resolve_h = CSSPixelLength::new(height as _);
    let resolve_radii = |radius: &BorderCornerRadius| -> Vec2 {
        Vec2 {
            x: scale * radius.0.width.0.resolve(resolve_w).px() as f64,
            y: scale * radius.0.height.0.resolve(resolve_h).px() as f64,
        }
    };
    let s_border = style.get_border();
    let border_radii = NonUniformRoundedRectRadii {
        top_left: resolve_radii(&s_border.border_top_left_radius),
        top_right: resolve_radii(&s_border.border_top_right_radius),
        bottom_right: resolve_radii(&s_border.border_bottom_right_radius),
        bottom_left: resolve_radii(&s_border.border_bottom_left_radius),
    };

    CssBox::new(border_box, border, padding, outline_width, border_radii)
}

#[cfg(feature = "svg")]
/// acrender: draws a usvg tree with `inner` (relative to the element) and the element's `transform`
/// applied on top. svg.rs (like anyrender_svg before it) outsets group-opacity clips by 2px after transforming to the canvas,
/// so rendering with the full transform made that margin depend on where the element was drawn
/// (a cached paint fragment is recorded in local space and placed later): render relative to the
/// element and append, so it is the same either way.
fn draw_svg_tree(scene: &mut impl PaintScene, tree: &usvg::Tree, transform: Affine, inner: Affine) {
    let mut local = acde_gpu::Scene::new();
    crate::svg::SvgPainter::draw(&mut local, tree, inner);
    scene.append_scene(local, transform);
}

/// acrender: paint a simple inline SVG (see blitz-dom's simple_svg.rs)
#[cfg(feature = "svg")]
fn draw_simple_svg(
    scene: &mut impl PaintScene,
    svg: &blitz_dom::node::SimpleSvg,
    transform: Affine,
) {
    use blitz_dom::node::SimpleOp;
    let color = |c: &[f32; 4]| peniko::Color::new(*c);
    for op in &svg.ops {
        match op {
            SimpleOp::Fill {
                path,
                transform: t,
                color: c,
                even_odd,
            } => {
                let rule = if *even_odd {
                    Fill::EvenOdd
                } else {
                    Fill::NonZero
                };
                scene.fill(rule, transform * *t, color(c), None, path);
            }
            SimpleOp::Stroke {
                path,
                transform: t,
                stroke,
                color: c,
            } => {
                scene.stroke(stroke, transform * *t, color(c), None, path);
            }
            SimpleOp::Tree { tree, transform: t } => draw_svg_tree(scene, tree, transform, *t),
            // acrender: clipped to what the group draws (a ±1e6 clip left whole icons unpainted)
            SimpleOp::PushOpacity { alpha, bounds } => {
                scene.push_layer(
                    Fill::NonZero,
                    peniko::Mix::Normal,
                    *alpha,
                    transform,
                    bounds,
                    None,
                    None,
                );
            }
            SimpleOp::Pop => scene.pop_layer(),
        }
    }
}
