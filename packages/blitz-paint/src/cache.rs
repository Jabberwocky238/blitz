//! acrender: paint fragment cache (incremental recording).
//!
//! The scene is recorded per *paint boundary* (the root element, stacking contexts, clipping /
//! scrolling containers, images and SVGs). A boundary's fragment holds its own commands, in the
//! boundary's local space (its border box at the origin, before its own transform), and references
//! to the boundary children painted inside it, at the command index where they paint. A frame is
//! the root fragment flattened through those references; only fragments whose inputs changed are
//! re-recorded.
//!
//! A fragment is reused while every element painted into it still has the same style (`Arc`
//! identity), layout box, scroll offset, element state, `paint_epoch` (replaced images / SVG data),
//! text layout generation, child list, hoisted-child positions, selection and scrollbar opacity, and
//! its references still point at boundaries. Form controls, iframes, canvases and custom widgets
//! are re-recorded every frame.
use std::collections::HashMap;

use acgpu::Scene;
use acgpu::RenderCommand;
use blitz_dom::node::{NodeData, SpecialElementData};
use blitz_dom::{BaseDocument, Node, NodeId};
use kurbo::{Affine, Point, Rect, Vec2};
use style::computed_values::overflow_x::T as Overflow;
use style::properties::ComputedValues;
use style::servo_arc::Arc;
use taffy::Layout;

/// What the painter saw of one element while recording a fragment.
struct Dep {
    node: NodeId,
    /// The fragment's own boundary: its location is applied when flattening, so it is not compared
    own: bool,
    /// The root element: the viewport scroll is compared too
    root: bool,
    style: Option<Arc<ComputedValues>>,
    layout: Layout,
    scroll: (f64, f64),
    state: u64,
    epoch: u64,
    text: u64,
    children: Option<Vec<NodeId>>,
    hoisted: Vec<(NodeId, (f32, f32))>,
    selection: Option<(usize, usize)>,
    scrollbar: f32,
    viewport_scroll: Option<(f64, f64)>,
    /// Culled: its scrollable overflow (which takes in its descendants, not visited), compared
    /// since a descendant can move into view on its own
    overflow: Option<Rect>,
}

/// acrender: inputs of an element that a Dep records and `holds` compares
fn scroll_of(node: &Node) -> (f64, f64) {
    node.element_data().map_or((0.0, 0.0), |_| {
        (node.scroll_offset().x, node.scroll_offset().y)
    })
}

fn state_of(node: &Node) -> (u64, u64, u64) {
    node.element_data().map_or((0, 0, 0), |e| {
        let text = e.inline_layout_data.as_ref().map_or(0, |t| t.generation);
        (e.element_state.bits(), e.paint_epoch, text)
    })
}

fn hoisted_of<'a>(
    doc: &'a BaseDocument,
    node: &'a Node,
) -> impl Iterator<Item = (NodeId, (f32, f32))> + 'a {
    let tree = doc.as_ref().tree();
    node.stacking_context.iter().flat_map(move |sc| {
        sc.neg_z_hoisted_children()
            .chain(sc.pos_z_hoisted_children())
            .map(move |c| {
                let p = c.position(tree, node.id);
                (c.node_id, (p.x, p.y))
            })
    })
}

fn viewport_scroll_of(doc: &BaseDocument) -> (f64, f64) {
    let s = doc.as_ref().viewport_scroll();
    (s.x, s.y)
}

impl Dep {
    fn of(doc: &BaseDocument, node: &Node, own: bool, selection: Option<(usize, usize)>) -> Self {
        let root = doc.try_root_element().map(|r| r.id) == Some(node.id);
        let (state, epoch, text) = state_of(node);
        Self {
            node: node.id,
            own,
            root,
            style: node.primary_styles().map(|s| (*s).clone()),
            layout: *node.final_layout(),
            scroll: scroll_of(node),
            state,
            epoch,
            text,
            children: node.paint_children.borrow().as_ref().map(|c| c.to_vec()),
            hoisted: hoisted_of(doc, node).collect(),
            selection,
            scrollbar: doc.scrollbar_opacity(node.id),
            viewport_scroll: root.then(|| viewport_scroll_of(doc)),
            overflow: None,
        }
    }

    /// acrender: compared in place, cheapest first (this runs for every element of every reused
    /// fragment, every frame; building a fresh Dep cloned its child list and hoisted children)
    fn holds(&self, doc: &BaseDocument, selection: &HashMap<NodeId, (usize, usize)>) -> bool {
        let Some(node) = doc.get_node(self.node) else {
            return false;
        };
        let same_style = match (&self.style, node.primary_styles()) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, &b),
            (None, None) => true,
            _ => false,
        };
        let now = node.final_layout();
        let mut layout = self.layout;
        if self.own {
            layout.location = now.location;
        }
        same_style
            && layout == *now
            && (self.state, self.epoch, self.text) == state_of(node)
            && self.scroll == scroll_of(node)
            && self.selection == selection.get(&self.node).copied()
            && self.scrollbar == doc.scrollbar_opacity(node.id)
            && self.viewport_scroll == self.root.then(|| viewport_scroll_of(doc))
            && self.children.as_deref() == node.paint_children.borrow().as_deref()
            && self.hoisted.iter().copied().eq(hoisted_of(doc, node))
            && self
                .overflow
                .is_none_or(|o| o == *node.scrollable_overflow())
    }
}

/// A boundary child painted inside a fragment: at command index `at`, with the transform the parent
/// passed it (the child's own location is applied on top when flattening).
#[derive(Clone, Copy)]
pub(crate) struct Ref {
    pub(crate) at: usize,
    pub(crate) node: NodeId,
    pub(crate) transform: Affine,
}

pub(crate) struct Fragment {
    pub(crate) commands: std::sync::Arc<[RenderCommand]>,
    pub(crate) refs: Vec<Ref>,
    deps: Vec<Dep>,
    volatile: bool,
    last_used: u64,
    /// Where it was recorded (local space → scene), whether culling left anything out there, and
    /// the bounds of what it paints (local space): a fragment that culled something is only right
    /// at that placement; one that culled nothing, wherever all of it stays in view
    placement: Affine,
    culled: bool,
    content: Rect,
}

/// A fragment being recorded.
pub(crate) struct Builder {
    pub(crate) node: NodeId,
    pub(crate) refs: Vec<Ref>,
    deps: Vec<Dep>,
    pub(crate) placement: Affine,
    culled: bool,
    content: Rect,
}

impl Builder {
    pub(crate) fn new(node: NodeId, placement: Affine) -> Self {
        Self {
            node,
            refs: Vec::new(),
            deps: Vec::new(),
            placement,
            culled: false,
            content: Rect::ZERO,
        }
    }

    /// The element last visited was culled (with this scrollable overflow), or painted within
    /// `bounds` (local space).
    pub(crate) fn painted(&mut self, culled: Option<Rect>, bounds: Rect) {
        if let Some(overflow) = culled {
            self.culled = true;
            if let Some(dep) = self.deps.last_mut() {
                dep.overflow = Some(overflow);
            }
        } else if self.content == Rect::ZERO {
            self.content = bounds;
        } else {
            self.content = self.content.union(bounds);
        }
    }

    /// Notes an element the painter is about to paint into this fragment.
    pub(crate) fn visit(
        &mut self,
        doc: &BaseDocument,
        node: &Node,
        selection: Option<(usize, usize)>,
    ) {
        let own = node.id == self.node;
        self.deps.push(Dep::of(doc, node, own, selection));
        self.visit_table(doc, node);
    }

    /// A table root paints its columns', rows' and their groups' backgrounds behind its cells, at
    /// the cells' boxes, so those are dependencies too: columns and column groups are never painted
    /// themselves, rows, row groups and cells only when they are not boundaries.
    fn visit_table(&mut self, doc: &BaseDocument, node: &Node) {
        let Some(SpecialElementData::TableRoot(table)) =
            node.element_data().map(|e| &e.special_data)
        else {
            return;
        };
        let mut ids: Vec<NodeId> = table
            .columns
            .iter()
            .flat_map(|c| [Some(c.node_id), c.group])
            .chain(table.rows.iter().flat_map(|r| [Some(r.node_id), r.group]))
            .flatten()
            .chain(table.cells.iter().map(|c| c.node_id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let Some(n) = doc.get_node(id) else { continue };
            let painted = n.primary_styles().is_some_and(|s| {
                use style::values::specified::box_::DisplayInside;
                !matches!(
                    s.get_box().display.inside(),
                    DisplayInside::TableColumn | DisplayInside::TableColumnGroup
                )
            });
            if !painted || PaintCache::is_boundary(n) {
                self.deps.push(Dep::of(doc, n, false, None));
            }
        }
    }
}

/// acrender: a slice of a fragment's commands in a frame, placed with `transform` (the frame
/// references the cached fragments instead of copying their commands).
#[derive(Clone)]
pub struct Piece {
    pub commands: std::sync::Arc<[RenderCommand]>,
    pub range: std::ops::Range<usize>,
    pub transform: Affine,
}

/// A run of the flattened frame's commands that come from one fragment: `len` commands at
/// `start` in the frame, `local` in the fragment.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    pub start: usize,
    pub len: usize,
    pub node: NodeId,
    pub local: usize,
}

/// How a fragment's commands in this frame relate to the previous frame's (see [`PaintCache::change`]).
pub enum Change<'c> {
    /// Reused at the same place: its commands are the same as last frame's
    Same,
    /// Placed elsewhere (re-recorded or not), newly painted, or gone: all of its commands changed
    Moved,
    /// Re-recorded at the same place: compare its old and new local commands
    Recorded {
        old: &'c [RenderCommand],
        new: &'c [RenderCommand],
    },
}

/// Counters of the last frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub recorded: u32,
    pub reused: u32,
    pub fragments: u32,
    pub commands: u32,
}

/// Paint fragments kept between frames (see the module docs). Owned by the caller of
/// [`crate::paint_scene_cached`].
pub struct PaintCache {
    fragments: HashMap<NodeId, Fragment>,
    /// Scale and scrollbar hover / drag state: anything else changing clears the cache
    key: Option<String>,
    frame: u64,
    /// Upper bound on the commands kept; past it the least recently used fragments go
    limit: usize,
    /// This paint's and the previous paint's runs and fragment placements (see [`Run`]); the
    /// previous ones are only meaningful while `continuous`
    runs: Vec<Run>,
    prev_runs: Vec<Run>,
    places: HashMap<NodeId, Affine>,
    prev_places: HashMap<NodeId, Affine>,
    continuous: bool,
    /// Fragments re-recorded this paint, with their old commands (`None`: new)
    recorded: HashMap<NodeId, Option<std::sync::Arc<[RenderCommand]>>>,
    pub stats: CacheStats,
}

impl Default for PaintCache {
    fn default() -> Self {
        Self::new(200_000)
    }
}

impl PaintCache {
    pub fn new(limit: usize) -> Self {
        Self {
            fragments: HashMap::new(),
            key: None,
            frame: 0,
            limit,
            runs: Vec::new(),
            prev_runs: Vec::new(),
            places: HashMap::new(),
            prev_places: HashMap::new(),
            continuous: false,
            recorded: HashMap::new(),
            stats: CacheStats::default(),
        }
    }

    pub fn clear(&mut self) {
        self.fragments.clear();
        self.key = None;
    }

    /// Starts a frame; a changed `key` drops everything.
    pub(crate) fn begin(&mut self, key: String) {
        self.continuous = self.key.as_ref() == Some(&key);
        if !self.continuous {
            self.fragments.clear();
            self.key = Some(key);
        }
        self.frame += 1;
        self.stats = CacheStats::default();
        self.prev_runs = std::mem::take(&mut self.runs);
        self.prev_places = std::mem::take(&mut self.places);
        self.recorded.clear();
    }

    /// Notes a run of `node`'s commands appended to the frame, and where `node` was placed.
    pub(crate) fn ran(&mut self, run: Run, place: Affine) {
        self.places.insert(run.node, place);
        if run.len > 0 {
            self.runs.push(run);
        }
    }

    /// This paint's number; [`Self::previous`] is the one before when its runs are comparable.
    pub fn sequence(&self) -> u64 {
        self.frame
    }

    pub fn previous(&self) -> Option<u64> {
        self.continuous.then(|| self.frame - 1)
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    pub fn previous_runs(&self) -> &[Run] {
        &self.prev_runs
    }

    /// How `node`'s commands changed since the previous paint.
    pub fn change(&self, node: NodeId) -> Change<'_> {
        let same_place = match (self.prev_places.get(&node), self.places.get(&node)) {
            (Some(a), Some(b)) => Self::close(*a, *b),
            _ => false,
        };
        match (same_place, self.recorded.get(&node)) {
            (false, _) => Change::Moved,
            (true, None) => Change::Same,
            // re-recorded in place: its old and new local commands can be compared directly
            (true, Some(old)) => match (old, self.fragments.get(&node)) {
                (Some(old), Some(new)) => Change::Recorded {
                    old,
                    new: &new.commands,
                },
                _ => Change::Moved,
            },
        }
    }

    /// Whether `node`'s fragment can be reused this frame (and marks it used).
    pub(crate) fn reuse(
        &mut self,
        doc: &BaseDocument,
        node: NodeId,
        (selection, pinned): (
            &HashMap<NodeId, (usize, usize)>,
            &std::collections::HashSet<NodeId>,
        ),
        placement: Affine,
        viewport: Rect,
    ) -> bool {
        let frame = self.frame;
        let Some(fragment) = self.fragments.get_mut(&node) else {
            return false;
        };
        let same_place = Self::close(fragment.placement, placement);
        let in_view = || {
            let r = placement.transform_rect_bbox(fragment.content);
            viewport.contains(r.origin()) && viewport.contains(Point::new(r.x1, r.y1))
        };
        let valid = !fragment.volatile
            && (same_place || (!fragment.culled && in_view()))
            && fragment.deps.iter().all(|d| d.holds(doc, selection))
            && fragment.refs.iter().all(|r| {
                pinned.contains(&r.node) || doc.get_node(r.node).is_some_and(Self::is_boundary)
            });
        if valid {
            fragment.last_used = frame;
            self.stats.reused += 1;
        }
        valid
    }

    pub(crate) fn store(
        &mut self,
        node: NodeId,
        scene: Scene,
        builder: Builder,
        doc: &BaseDocument,
    ) {
        let volatile = doc.get_node(node).is_some_and(Self::is_volatile);
        self.stats.recorded += 1;
        let Builder {
            refs,
            deps,
            placement,
            culled,
            content,
            ..
        } = builder;
        let last_used = self.frame;
        let old = self.fragments.insert(
            node,
            Fragment {
                commands: scene.commands.into(),
                refs,
                deps,
                volatile,
                last_used,
                placement,
                culled,
                content,
            },
        );
        self.recorded.entry(node).or_insert(old.map(|f| f.commands));
    }

    pub(crate) fn get(&self, node: NodeId) -> Option<&Fragment> {
        self.fragments.get(&node)
    }

    /// Ends a frame: drops fragments of removed nodes, then the least recently used ones while
    /// over the command limit (never ones used this frame).
    pub(crate) fn end(&mut self, doc: &BaseDocument) {
        self.fragments.retain(|id, _| doc.get_node(*id).is_some());
        let mut commands: usize = self.fragments.values().map(|f| f.commands.len()).sum();
        if commands > self.limit {
            let mut old: Vec<_> = self
                .fragments
                .iter()
                .filter(|(_, f)| f.last_used < self.frame)
                .map(|(id, f)| (f.last_used, *id))
                .collect();
            old.sort_unstable_by_key(|(used, _)| *used);
            for (_, id) in old {
                if commands <= self.limit {
                    break;
                }
                commands -= self.fragments.remove(&id).map_or(0, |f| f.commands.len());
            }
        }
        self.stats.fragments = self.fragments.len() as u32;
        self.stats.commands = commands as u32;
    }

    /// The fragment the last frame's `index`-th command came from.
    pub fn owner(&self, index: usize) -> Option<NodeId> {
        let i = self.runs.partition_point(|r| r.start <= index);
        i.checked_sub(1).map(|i| self.runs[i].node)
    }

    /// Whether `node` gets a fragment of its own.
    pub(crate) fn is_boundary(node: &Node) -> bool {
        let Some(element) = node.element_data() else {
            return false;
        };
        let clips = node.primary_styles().is_some_and(|s| {
            let b = s.get_box();
            !matches!(b.overflow_x, Overflow::Visible) || !matches!(b.overflow_y, Overflow::Visible)
        });
        node.stacking_context.is_some()
            || clips
            || !matches!(element.special_data, SpecialElementData::None | SpecialElementData::Stylesheet(_))
            || node.parent.is_some_and(|p| matches!(node.with(p).data, NodeData::Document(_)))
            || Self::list_item(node)
            // acrender: a 2D canvas's picture changes on its own (repaint), without re-recording its parent
            || &*element.name.local == "canvas"
    }

    /// A block-level child of an element with many children (rows of a list, messages, cards):
    /// on its own, a change in one of them doesn't re-record its siblings.
    fn list_item(node: &Node) -> bool {
        const MANY: usize = 16;
        let block = node.primary_styles().is_some_and(|s| {
            use style::values::specified::box_::DisplayOutside;
            s.get_box().display.outside() == DisplayOutside::Block
        });
        block
            && node
                .parent
                .is_some_and(|p| node.with(p).children.len() >= MANY)
    }

    /// Boundaries whose painting depends on state the dependencies don't see.
    /// acrender: not table roots: what they paint from the table comes in as dependencies
    /// (Builder::visit_table), and a rebuilt table context bumps `paint_epoch`
    fn is_volatile(node: &Node) -> bool {
        node.element_data().is_some_and(|e| {
            !matches!(
                e.special_data,
                SpecialElementData::None
                    | SpecialElementData::Stylesheet(_)
                    | SpecialElementData::Image(_)
                    | SpecialElementData::TableRoot(_)
            )
        })
    }

    fn close(a: Affine, b: Affine) -> bool {
        a.as_coeffs()
            .iter()
            .zip(b.as_coeffs())
            .all(|(x, y)| (x - y).abs() < 1e-6)
    }

    /// Where a boundary child's commands go in its parent's space: the parent's transform for it,
    /// then the child's (current) location.
    pub(crate) fn place(node: &Node, transform: Affine, scale: f64) -> Affine {
        let location = node.final_layout().location;
        transform * Affine::translate(Vec2::new(location.x as f64, location.y as f64) * scale)
    }
}
