use acde_gpu::PaintScene;
use blitz_dom::{BaseDocument, NodeId, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, BezPath, Cap, Circle, Rect, Stroke};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::collections::HashMap;
use style::properties::generated::longhands::text_decoration_style::computed_value::T as TextDecorationStyle;
use style::values::computed::{
    Length, LengthPercentage, TextDecorationLength, TextDecorationLine, TextUnderlinePosition,
};
use style::values::generics::text::{GenericTextDecorationInset, GenericTextDecorationLength};

use crate::color::{Color, ToColorColor as _};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// Draw the backgrounds of inline elements (e.g. `<span style="background: ...">`).
///
/// Each glyph run carries the node id of the innermost inline element it belongs to
/// (via its brush). We look up that node's `background-color` and, if non-transparent,
/// fill a rectangle covering the run's advance and its font's ascent/descent so that the
/// background sits behind the text.
///
/// acrender: an element with inline edges (padding / border / margin, or a rounded background; see
/// blitz-dom `InlineEdge`) is drawn per line between its edge boxes instead: the padding box (the
/// runs' content area grown by the vertical padding and border) with its background, rounded
/// corners and border only at the ends that are on this line (`box-decoration-break: slice`).
///
/// The inline root's own background is painted separately (as a normal block box), so
/// runs belonging to the root are skipped to avoid drawing it twice.
pub(crate) fn draw_inline_backgrounds<'a, 'dom>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    painter: &'dom crate::render::BlitzDomPainter<'dom, '_>,
    transform: Affine,
    inline_root_id: NodeId,
) {
    let doc = painter.dom;
    use blitz_dom::node::InlineEdge;
    // elements open at the end of the previous line (they continue on this one), in opening order
    let mut carried: Vec<(NodeId, u64)> = Vec::new();
    let mut fragments: Vec<InlineFragment> = Vec::new();
    let mut opened = 0u64;
    for line in lines {
        let metrics = line.metrics();
        let mut open: Vec<InlineFragment> = carried
            .drain(..)
            .map(|(node, order)| {
                InlineFragment::new(node, None, metrics.inline_min_coord as f64, order)
            })
            .collect();
        for item in line.items() {
            match item {
                PositionedLayoutItem::InlineBox(ibox) => {
                    let Some(edge) = InlineEdge::of(ibox.id) else {
                        continue;
                    };
                    let (x0, x1) = (ibox.x as f64, (ibox.x + ibox.width) as f64);
                    match edge.end {
                        false => {
                            opened += 1;
                            open.push(InlineFragment::new(edge.node, Some(x0), x1, opened));
                        }
                        true => {
                            if let Some(at) = open.iter().rposition(|f| f.node == edge.node) {
                                let mut fragment = open.remove(at);
                                fragment.end = Some(x1);
                                fragment.right = fragment.right.max(x0);
                                fragments.push(fragment);
                            }
                        }
                    }
                }
                PositionedLayoutItem::GlyphRun(glyph_run) => {
                    let run_metrics = glyph_run.run().font_metrics();
                    let baseline = glyph_run.baseline() as f64;
                    let (x0, x1) = (
                        glyph_run.offset() as f64,
                        (glyph_run.offset() + glyph_run.advance()) as f64,
                    );
                    for fragment in &mut open {
                        fragment.cover(
                            x0,
                            x1,
                            baseline - run_metrics.ascent as f64,
                            baseline + run_metrics.descent as f64,
                        );
                    }
                    let node_id = glyph_run.style().brush.id;
                    if node_id == inline_root_id || open.iter().any(|f| f.node == node_id) {
                        continue;
                    }
                    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles())
                    else {
                        continue;
                    };
                    let current_color = styles.clone_color();
                    let bg_color = styles
                        .get_background()
                        .background_color
                        .resolve_to_absolute(&current_color)
                        .as_srgb_color();
                    if bg_color == Color::TRANSPARENT {
                        continue;
                    }
                    let y0 = baseline - run_metrics.ascent as f64;
                    let y1 = baseline + run_metrics.descent as f64;
                    scene.fill(
                        Fill::NonZero,
                        transform,
                        bg_color,
                        None,
                        &Rect::new(x0, y0, x1, y1),
                    );
                }
            }
        }
        // still open at the end of the line: sliced here, continues on the next one
        for fragment in open {
            carried.push((fragment.node, fragment.order));
            fragments.push(fragment);
        }
    }
    // outer elements first (they opened first), so nested backgrounds sit on top
    fragments.sort_by_key(|f| f.order);
    // percentages of an inline element's padding / margin are of its containing block's width (the
    // inline root's content box)
    let basis = doc.get_node(inline_root_id).map_or(0.0, |root| {
        let l = root.final_layout();
        l.size.width - l.padding.left - l.padding.right - l.border.left - l.border.right
    });
    for fragment in &fragments {
        fragment.draw(scene, painter, transform, basis);
    }
}

/// acrender: one line's piece of an inline element with edges (see draw_inline_backgrounds)
struct InlineFragment {
    node: NodeId,
    /// The start / end edge box's outer x when that edge is on this line
    start: Option<f64>,
    end: Option<f64>,
    /// Content extent seen on this line (x of the first / last content, line-relative)
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    /// When the element opened (outer ones first)
    order: u64,
}

impl InlineFragment {
    fn new(node: NodeId, start: Option<f64>, x: f64, order: u64) -> Self {
        Self {
            node,
            start,
            end: None,
            left: x,
            right: x,
            top: f64::INFINITY,
            bottom: f64::NEG_INFINITY,
            order,
        }
    }

    fn cover(&mut self, x0: f64, x1: f64, y0: f64, y1: f64) {
        if self.top > self.bottom {
            self.left = x0;
        }
        self.right = self.right.max(x1);
        self.top = self.top.min(y0);
        self.bottom = self.bottom.max(y1);
    }

    /// basis: the containing block's width (CSS px), for percentage padding / margin
    fn draw<'dom>(
        &self,
        scene: &mut impl PaintScene,
        painter: &'dom crate::render::BlitzDomPainter<'dom, '_>,
        transform: Affine,
        basis: f32,
    ) {
        use crate::kurbo_css::{CssBox, NonUniformRoundedRectRadii};
        use kurbo::{Insets, Vec2};
        let Some(node) = painter.dom.get_node(self.node) else {
            return;
        };
        let Some(style) = node.primary_styles() else {
            return;
        };
        if self.top > self.bottom {
            return;
        }
        let scale = painter.dom.viewport().scale_f64();
        let px = |l: &style::values::computed::LengthPercentage| {
            l.resolve(Length::new(basis)).px() as f64 * scale
        };
        let (p, m, b) = (style.get_padding(), style.get_margin(), style.get_border());
        let margin = |v: &style::values::computed::Margin| match v {
            style::values::generics::length::GenericMargin::LengthPercentage(l) => px(l),
            _ => 0.0,
        };
        let width =
            |w: &style::values::computed::BorderSideWidth,
             s: style::values::computed::BorderStyle| match s.none_or_hidden() {
                true => 0.0,
                false => w.0.to_f64_px() * scale,
            };
        let (at_start, at_end) = (self.start.is_some(), self.end.is_some());
        // the left / right sides only at the ends that are on this line (`box-decoration-break: slice`)
        let border = Insets {
            x0: if at_start {
                width(&b.border_left_width, b.border_left_style)
            } else {
                0.0
            },
            y0: width(&b.border_top_width, b.border_top_style),
            x1: if at_end {
                width(&b.border_right_width, b.border_right_style)
            } else {
                0.0
            },
            y1: width(&b.border_bottom_width, b.border_bottom_style),
        };
        let padding = Insets {
            x0: if at_start { px(&p.padding_left.0) } else { 0.0 },
            y0: px(&p.padding_top.0),
            x1: if at_end { px(&p.padding_right.0) } else { 0.0 },
            y1: px(&p.padding_bottom.0),
        };
        // border box: the edge boxes hold margin + border + padding; vertically the content area grown
        let x0 = self.start.map_or(self.left, |x| x + margin(&m.margin_left));
        let x1 = self.end.map_or(self.right, |x| x - margin(&m.margin_right));
        let y0 = self.top - padding.y0 - border.y0;
        let y1 = self.bottom + padding.y1 + border.y1;
        if x1 <= x0 {
            return;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let radius = |r: &style::values::computed::BorderCornerRadius, on: bool| match on {
            true => Vec2::new(
                r.0.width.0.resolve(Length::new((w / scale) as f32)).px() as f64 * scale,
                r.0.height.0.resolve(Length::new((h / scale) as f32)).px() as f64 * scale,
            ),
            false => Vec2::ZERO,
        };
        let radii = NonUniformRoundedRectRadii {
            top_left: radius(&b.border_top_left_radius, at_start),
            top_right: radius(&b.border_top_right_radius, at_end),
            bottom_right: radius(&b.border_bottom_right_radius, at_end),
            bottom_left: radius(&b.border_bottom_left_radius, at_start),
        };
        let frame = CssBox::new(Rect::new(0.0, 0.0, w, h), border, padding, 0.0, radii);
        painter.draw_inline_fragment(scene, node, frame, transform * Affine::translate((x0, y0)));
    }
}

/// Per-font-face cache of the OS/2 `usWinAscent / unitsPerEm` ratio, keyed by the font
/// blob's unique id and face index. `None` is cached for fonts without a usable OS/2 or
/// head table so they aren't re-parsed either.
type WinAscentCache = HashMap<(u64, u32), Option<f32>>;

/// The font's OS/2 `usWinAscent`, in the same (device) pixels as `font_size`.
///
/// This is the ascent browsers use as the top of the "em box" when positioning overlines. It
/// is typically taller than the hhea ascent Parley exposes via [`parley::layout::run::RunMetrics`].
/// Returns `None` when the font has no OS/2 table.
fn win_ascent(cache: &mut WinAscentCache, font: &parley::FontData, font_size: f32) -> Option<f32> {
    let ratio = *cache
        .entry((font.data.id(), font.index))
        .or_insert_with(|| win_ascent_ratio(font));
    Some(ratio? * font_size)
}

/// The unitless `usWinAscent / unitsPerEm` ratio for a font face, or `None` when the font
/// has no usable head/OS/2 table.
fn win_ascent_ratio(font: &parley::FontData) -> Option<f32> {
    use skrifa::raw::{FontRef, TableProvider as _};
    let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
    let units_per_em = font_ref.head().ok()?.units_per_em();
    let win_ascent = font_ref.os2().ok()?.us_win_ascent();
    Some(win_ascent as f32 / units_per_em as f32)
}

/// Mirrors Blink's `SelectBestDashGap`: choose the gap length (as close as possible to
/// `gap_length`) that fits a whole number of `dash_length` dashes across `stroke_length`,
/// so dashes are evenly distributed and a dash lands at each end of the line.
fn select_best_dash_gap(stroke_length: f64, dash_length: f64, gap_length: f64) -> f64 {
    let available_length = stroke_length + gap_length;
    let min_num_dashes = (available_length / (dash_length + gap_length)).floor();
    let max_num_dashes = min_num_dashes + 1.0;
    let min_num_gaps = min_num_dashes - 1.0;
    let max_num_gaps = max_num_dashes - 1.0;
    if min_num_gaps < 1.0 {
        return gap_length;
    }
    let min_gap = (stroke_length - min_num_dashes * dash_length) / min_num_gaps;
    let max_gap = (stroke_length - max_num_dashes * dash_length) / max_num_gaps;
    if max_gap <= 0.0 || (min_gap - gap_length).abs() < (max_gap - gap_length).abs() {
        min_gap
    } else {
        max_gap
    }
}

/// A text decoration resolved from a single "decorating box" (an element that
/// sets `text-decoration-line`), ready to be drawn over a glyph run. Because the
/// `text-decoration-*` properties do not inherit, decorations are propagated from
/// ancestors rather than inherited, so a run may be covered by several of these.
///
/// Only the run-independent (style-derived) values are stored here so an instance
/// can be cached on the ancestor stack and reused across every run within the same
/// decorating box. The `text-decoration-inset` and `text-underline-offset` values
/// depend on the run's advance/font-size and are resolved lazily when drawing.
#[derive(Clone)]
struct ResolvedDecoration {
    line: TextDecorationLine,
    style: TextDecorationStyle,
    color: Color,
    thickness: TextDecorationLength,
    /// `text-underline-offset`; `None` for `auto`. Resolved per run.
    underline_offset: Option<LengthPercentage>,
    /// Whether `text-underline-position: under` is in effect.
    underline_under: bool,
    /// Whether `text-underline-position: from-font` is in effect.
    underline_from_font: bool,
    /// `text-decoration-inset`. Resolved per run.
    inset: GenericTextDecorationInset<LengthPercentage>,
}

/// An element on the current ancestor path (inline root -> run node), caching the
/// values resolved from its computed style so descending into the same node across
/// consecutive runs doesn't re-resolve them.
struct DecorationStackEntry {
    node_id: NodeId,
    /// This node's (inherited) text colour, used for the glyphs of runs whose
    /// innermost node is this one.
    text_color: Color,
    /// The decoration this node introduces as a decorating box, if any.
    decoration: Option<ResolvedDecoration>,
}

/// Resolve the cached style values for a single node into a [`DecorationStackEntry`].
fn resolve_decoration_entry(doc: &BaseDocument, node_id: NodeId) -> DecorationStackEntry {
    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
        return DecorationStackEntry {
            node_id,
            text_color: Color::BLACK,
            decoration: None,
        };
    };

    let itext = styles.get_inherited_text();
    let text = styles.get_text();
    let text_color = itext.color.as_color_color();

    let drawn_lines = TextDecorationLine::UNDERLINE
        | TextDecorationLine::OVERLINE
        | TextDecorationLine::LINE_THROUGH;
    let line = text.text_decoration_line;
    // Decorations propagate through the box tree, and a `display: contents` element
    // generates no box, so its decorations have no effect on descendants.
    let is_contents = styles.clone_display().is_contents();
    let decoration = (!is_contents && line.intersects(drawn_lines)).then(|| {
        // `text-decoration-color: currentColor` (the initial value) resolves against
        // the decorating box's own colour, not the descendant run's.
        let color = text
            .text_decoration_color
            .as_absolute()
            .map(ToColorColor::as_color_color)
            .unwrap_or(text_color);

        ResolvedDecoration {
            line,
            style: text.text_decoration_style,
            color,
            thickness: text.text_decoration_thickness.clone(),
            underline_offset: itext.text_underline_offset.non_auto(),
            underline_under: itext
                .text_underline_position
                .contains(TextUnderlinePosition::UNDER),
            underline_from_font: itext
                .text_underline_position
                .contains(TextUnderlinePosition::FROM_FONT),
            inset: text.text_decoration_inset.clone(),
        }
    });

    DecorationStackEntry {
        node_id,
        text_color,
        decoration,
    }
}

/// The run-dependent geometry used to position and size a decoration, captured from a
/// single glyph run. Firefox draws a decoration once for the whole decorating box using
/// the box's own font, so we prefer the values from a run whose innermost node *is* the
/// decorating box (its own text), falling back to the first run it covers.
#[derive(Clone)]
struct DecorationRunGeometry {
    /// The baseline the decoration is positioned from (the run's own, `vertical-align`
    /// shifted, baseline).
    baseline: f32,
    ascent: f32,
    descent: f32,
    underline_offset: f32,
    underline_size: f32,
    strikethrough_size: f32,
    font: parley::FontData,
    font_size: f32,
    css_font_size: f64,
}

/// A decoration accumulated across the runs of a single line for one decorating box.
///
/// The `text-decoration-*` properties describe the whole box, so we gather the horizontal
/// extent it covers on the line (`min_x`/`max_x`) and the font geometry to draw with, then
/// paint a single line spanning the box rather than one segment per run.
struct LineDecoration {
    node_id: NodeId,
    deco: ResolvedDecoration,
    min_x: f64,
    max_x: f64,
    /// Geometry from a run whose innermost node is the decorating box itself.
    own: Option<DecorationRunGeometry>,
    /// Geometry from the first run the box covers (fallback when it has no own text on
    /// this line, e.g. it only wraps differently-sized descendants).
    first: Option<DecorationRunGeometry>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    win_ascent_ratios: WinAscentCache,
    /// acrender: glyph colour for the next stroke_text instead of the runs' own (an input's placeholder)
    pub(crate) color: Option<Color>,
}

/// Resolve the CSS `text-decoration-thickness` to a device-pixel size.
///
/// - Percentages are resolved against the font size,
/// - `from-font` uses the metrics from Parley,
/// - `auto` uses a thickness of `font-size / 10` (minimum: 1px).
///
/// The result is floored to a whole number of device pixels (minimum 1), matching
/// browsers, which snap decoration thickness to whole pixels.
fn decoration_size(
    thickness: &TextDecorationLength,
    metric_size: f32,
    css_font_size: f64,
    scale: f64,
) -> f32 {
    (match thickness {
        GenericTextDecorationLength::LengthPercentage(lp) => {
            lp.resolve(Length::new(css_font_size as f32)).px() as f64 * scale
        }
        GenericTextDecorationLength::FromFont => metric_size as f64,
        GenericTextDecorationLength::Auto => (css_font_size / 10.0).max(1.0) * scale,
    })
    .floor()
    .max(1.0) as f32
}

/// Draws a single decoration line of the given `text-decoration-style` spanning
/// `[x0, x0 + width]`. `baseline` is the line's baseline and `offset` positions the line
/// relative to it (`y = baseline - offset + size / 2`, growing downward). Geometry is built
/// explicitly (rather than relying on backend dash support) so it renders consistently
/// across renderers.
///
/// `double_dir` points away from the text (`+1` = down, `-1` = up) and is used to place the
/// second line of a `double` decoration.
#[allow(clippy::too_many_arguments)]
fn draw_decoration_line(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    x0: f64,
    width: f64,
    baseline: f32,
    offset: f32,
    size: f32,
    brush: &acde_gpu::Paint,
    double_dir: f64,
    deco_style: TextDecorationStyle,
    inset_start: f64,
    inset_end: f64,
) {
    // Inset the line from the box's start/end edges (assumes LTR: start is the left edge).
    // Negative insets extend the line past the text.
    let x = x0 + inset_start;
    let w = width - inset_start - inset_end;
    if w <= 0.0 {
        return;
    }
    let y = (baseline - offset + size / 2.0) as f64;
    let size = size as f64;
    let butt_stroke = Stroke::new(size).with_caps(Cap::Butt);

    match deco_style {
        TextDecorationStyle::MozNone => {
            // no line. Equivalent to `text-decoration-line: none`
        }
        // `solid`
        TextDecorationStyle::Solid => {
            let line = kurbo::Line::new((x, y), (x + w, y));
            scene.stroke(&butt_stroke, transform, brush, None, &line);
        }
        // Two lines, each `size` thick, separated by a 1px (CSS) gap. This
        // matches Chrome, where the second line is offset by `thickness + 1px`
        // and sits further from the text (below for underline, above for
        // overline).
        TextDecorationStyle::Double => {
            let one_css_px = scale;
            for cy in [y, y + double_dir * (size + one_css_px)] {
                let line = kurbo::Line::new((x, cy), (x + w, cy));
                scene.stroke(&butt_stroke, transform, brush, None, &line);
            }
        }
        // Round dots (diameter = line thickness) spaced one dot apart.
        TextDecorationStyle::Dotted => {
            let radius = size / 2.0;
            let step = 2.0 * size;
            let mut cx = x + radius;
            while cx <= x + w - radius {
                let dot = Circle::new((cx, y), radius);
                scene.fill(Fill::NonZero, transform, brush, None, &dot);
                cx += step;
            }
        }
        // Dashes as filled rectangles. Dash/gap lengths are relative to the
        // thickness (matching Blink): thinner lines use proportionally longer
        // dashes and gaps so they don't read as dotted or solid.
        TextDecorationStyle::Dashed => {
            let (dash_ratio, gap_ratio) = if size >= 3.0 { (2.0, 1.0) } else { (3.0, 2.0) };
            let dash = size * dash_ratio;
            let nominal_gap = size * gap_ratio;
            // Nudge the gap so a whole number of dashes fits the run evenly.
            let gap = if w > 2.0 * dash {
                select_best_dash_gap(w, dash, nominal_gap)
            } else {
                nominal_gap
            };
            let mut dx = x;
            while dx < x + w {
                let end = (dx + dash).min(x + w);
                let rect = Rect::new(dx, y - size / 2.0, end, y + size / 2.0);
                scene.fill(Fill::NonZero, transform, brush, None, &rect);
                dx += dash + gap;
            }
        }
        // A squiggle built from alternating quadratic Béziers.
        TextDecorationStyle::Wavy => {
            let amplitude = size;
            let half_wave = (2.0 * size).max(1.0);
            let mut path = BezPath::new();
            path.move_to((x, y));
            let mut px = x;
            let mut up = true;
            while px < x + w {
                let nx = (px + half_wave).min(x + w);
                // A quad Bézier reaches half the control-point offset at its
                // midpoint, so use `2 * amplitude` to hit the target peak.
                let ctrl_y = if up {
                    y - 2.0 * amplitude
                } else {
                    y + 2.0 * amplitude
                };
                path.quad_to(((px + nx) / 2.0, ctrl_y), (nx, y));
                px = nx;
                up = !up;
            }
            let stroke = Stroke::new(size).with_caps(Cap::Round);
            scene.stroke(&stroke, transform, brush, None, &path);
        }
    }
}

/// Paint the decorations accumulated for one line, one line per decorating box.
fn flush_line_decorations(
    scene: &mut impl PaintScene,
    transform: Affine,
    scale: f64,
    deco_boxes: &[LineDecoration],
    win_ascent_ratios: &mut WinAscentCache,
    inline_root_id: NodeId,
    line_baseline: f32,
) {
    // Draw innermost boxes first so ancestors' decorations paint on top, matching the
    // per-run drawing order this replaced (`stack.iter().rev()`).
    for acc in deco_boxes.iter().rev() {
        let deco = &acc.deco;
        // Prefer the decorating box's own font; fall back to the first run it covers.
        let geom = match (&acc.own, &acc.first) {
            (Some(own), _) => own.clone(),
            (None, Some(first)) => {
                let mut geom = first.clone();
                // A descendant run's baseline may be shifted by `vertical-align`, but the
                // decoration is positioned from the decorating box's own baseline. For the
                // inline root that is the line's baseline.
                if acc.node_id == inline_root_id {
                    geom.baseline = line_baseline;
                }
                geom
            }
            (None, None) => continue,
        };
        let geom = &geom;
        let width = acc.max_x - acc.min_x;
        if width <= 0.0 {
            continue;
        }
        let brush = acde_gpu::Paint::from(deco.color);

        // `text-decoration-inset` shortens (or, when negative, extends) the line from the
        // inline-start/end edges. Percentages resolve against the decoration line length
        // (the box's total advance on this line); `auto` is no inset.
        let (inset_start, inset_end) = match &deco.inset {
            GenericTextDecorationInset::LengthPercentage { start, end } => {
                // The extent is in device pixels; convert to CSS pixels so the resolved
                // result can be scaled back.
                let line_length = Length::new((width / scale) as f32);
                (
                    start.resolve(line_length).px() as f64 * scale,
                    end.resolve(line_length).px() as f64 * scale,
                )
            }
            GenericTextDecorationInset::Auto => (0.0, 0.0),
        };

        if deco.line.contains(TextDecorationLine::UNDERLINE) {
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );

            // `text-underline-offset` moves the underline away from the text. `auto` keeps
            // the font's suggested position; otherwise it resolves against the font size
            // (percentages relative to 1em).
            let extra_underline_offset = deco
                .underline_offset
                .as_ref()
                .map(|lp| lp.resolve(Length::new(geom.css_font_size as f32)).px() as f64 * scale)
                .unwrap_or(0.0);

            // `text-underline-position: under` places the underline below the glyph box
            // (below descenders) rather than at the font's suggested position near the
            // alphabetic baseline. We anchor the top of the line at the descent so it clears
            // descending glyphs like "gqy".
            // An explicit offset under `auto` is taken from the alphabetic
            // baseline, as CSS and Chrome have it, not from the auto gap.
            let base_offset = if deco.underline_under {
                -geom.descent
            } else if deco.underline_offset.is_some() && !deco.underline_from_font {
                0.0
            } else {
                geom.underline_offset
            };
            let offset = base_offset - extra_underline_offset as f32;

            // A `double` underline extends downward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::OVERLINE) {
            // Fonts don't provide a dedicated overline metric, so reuse the underline
            // thickness. The line sits at the top of the "em box": its lower edge rests on
            // the ascent so it clears the glyphs, and it extends upward from there.
            //
            // Browsers use the OS/2 `usWinAscent` for this edge, which is taller than the
            // hhea-based ascent Parley reports; using the smaller value would draw the
            // overline too low. Fall back to Parley's ascent for fonts without a usable
            // OS/2 table.
            let size = decoration_size(
                &deco.thickness,
                geom.underline_size,
                geom.css_font_size,
                scale,
            );
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent + size;

            // A `double` overline extends upward, away from the text.
            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                -1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
        if deco.line.contains(TextDecorationLine::LINE_THROUGH) {
            let size = decoration_size(
                &deco.thickness,
                geom.strikethrough_size,
                geom.css_font_size,
                scale,
            );

            // Centre the line-through a third of the ascent above the baseline, matching
            // Chrome (which places it `2/3 * ascent` below the text-top). Parley's
            // `strikethrough_offset` (the font's `yStrikeoutPosition`) sits lower and would
            // draw the line too close to the baseline.
            let ascent =
                win_ascent(win_ascent_ratios, &geom.font, geom.font_size).unwrap_or(geom.ascent);
            let offset = ascent / 3.0 + size / 2.0;

            draw_decoration_line(
                scene,
                transform,
                scale,
                acc.min_x,
                width,
                geom.baseline,
                offset,
                size,
                &brush,
                1.0,
                deco.style,
                inset_start,
                inset_end,
            );
        }
    }
}

/// acrender: where lines end in "…" (`text-overflow: ellipsis`, `-webkit-line-clamp`).
pub(crate) struct Ellipsis<'a> {
    /// The box's content box end edge, in layout units.
    pub end: f32,
    /// "…" shaped in the box's own style.
    pub marker: &'a parley::Layout<TextBrush>,
    /// `text-overflow: ellipsis` applies: lines overflowing `end` are cut.
    pub overflow: bool,
    /// The last line `-webkit-line-clamp` keeps: it always ends in "…".
    pub clamp_line: Option<usize>,
    /// Where `text-align` puts that line once "…" is part of it: 0 at the left, 0.5 centred,
    /// 1 at the right.
    pub align: f32,
}

/// Synthetic emboldening for a run of `font_size` (physical px), when enabled.
/// acrender: and the synthetic bold a run gets when its font has no face as heavy as asked for (`synthetic`, from
/// fontique's synthesis), as Skia (Chrome) makes it: the outline grows by 1/24 of the size at 9 px or less, 1/32 at
/// 36 px or more and in between linearly (SkScalerContext's fake-bold keys), half of that on each side.
fn embolden(font_size: f32, scale: f64, synthetic: bool) -> kurbo::Vec2 {
    let fs = font_size as f64 / scale;
    let mut amount = match FONT_EMBOLDEN_ENABLED {
        true => kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3)),
        false => kurbo::Vec2::default(),
    };
    if synthetic {
        let ratio = 1.0 / 24.0 + (fs.clamp(9.0, 36.0) - 9.0) / 27.0 * (1.0 / 32.0 - 1.0 / 24.0);
        let each = font_size as f64 * ratio / 2.0;
        amount += kurbo::Vec2::new(each, each);
    }
    amount
}

/// acrender: where `run`'s glyphs that fit before `limit` end (the run's start if none does).
fn kept_end(run: &parley::GlyphRun<'_, TextBrush>, limit: f32) -> f32 {
    run.positioned_glyphs()
        .take_while(|g| g.x + g.advance <= limit)
        .last()
        .map_or(run.offset(), |g| g.x + g.advance)
}

/// acrender: draws the "…" marker run starting at `x` on the line with baseline `baseline`.
fn draw_ellipsis_marker(
    scene: &mut impl PaintScene,
    marker: &parley::GlyphRun<'_, TextBrush>,
    (x, baseline): (f32, f32),
    color: Color,
    transform: Affine,
    scale: f64,
) {
    let run = marker.run();
    let embolden = embolden(run.font_size(), scale, run.synthesis().embolden());
    let glyph_xform = run
        .synthesis()
        .skew()
        .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));
    let (dx, dy) = (x - marker.offset(), baseline - marker.baseline());
    scene.draw_glyphs(
        run.font(),
        run.font_size(),
        !FONT_EMBOLDEN_ENABLED,
        bytemuck::cast_slice(run.normalized_coords()),
        embolden,
        Fill::NonZero,
        &acde_gpu::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        marker.positioned_glyphs().map(|glyph| acde_gpu::Glyph {
            id: glyph.id as _,
            x: glyph.x + dx,
            y: glyph.y + dy,
        }),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
    inline_root_id: NodeId,
    ellipsis: Option<Ellipsis<'a>>,
    context: &mut DrawTextContext,
) {
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        win_ascent_ratios,
        color,
    } = context;
    let color = color.take();
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();

    // acrender: `text-overflow: ellipsis`: the box's end edge and the "…" marker's glyph run
    let ellipsis = ellipsis.and_then(|e| {
        let run = e
            .marker
            .lines()
            .next()?
            .items()
            .find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => Some(run),
                _ => None,
            })?;
        Some((e, run))
    });

    // Persistent stack mirroring the ancestor path (inline root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    let last_line = ellipsis.as_ref().and_then(|(e, _)| e.clamp_line);
    for (index, line) in lines.enumerate() {
        // acrender: lines after the one `-webkit-line-clamp` ends on are not painted (as in Firefox;
        // clipping alone leaves the tops of tall glyphs, e.g. CJK fallback fonts, showing)
        if last_line.is_some_and(|last| index > last) {
            break;
        }
        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();

        // acrender: on a line that overflows the box, glyphs from the first one that no longer leaves room
        // for "…" onwards are dropped, and "…" drawn after the last glyph kept.
        let line_end =
            line.metrics().offset + line.metrics().advance - line.metrics().hanging_advance;
        let clamped = ellipsis
            .as_ref()
            .is_some_and(|(e, _)| e.clamp_line == Some(index));
        let mut cut = ellipsis
            .as_ref()
            .filter(|(e, _)| clamped || (e.overflow && line_end > e.end + 0.5))
            .map(|(e, marker)| e.end - marker.advance());

        // acrender: the clamped line is re-aligned with "…" as part of it (as Firefox lays it out)
        let transform = match (clamped, cut, &ellipsis) {
            (true, Some(limit), Some((e, marker))) => {
                let kept_end = line
                    .items()
                    .find_map(|item| match item {
                        PositionedLayoutItem::GlyphRun(run)
                            if run.offset() + run.advance() > limit =>
                        {
                            Some(kept_end(&run, limit))
                        }
                        _ => None,
                    })
                    .unwrap_or(line_end);
                let start = line.metrics().offset;
                let width = kept_end + marker.advance() - start;
                let shift = (e.end - width).max(0.0) * e.align - start;
                transform * Affine::translate((shift as f64, 0.0))
            }
            _ => transform,
        };

        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                if cut.is_some_and(f32::is_nan) {
                    continue;
                }
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.font_metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                let css_font_size = font_size as f64 / scale;

                // Reconcile the stack with this run's ancestor path. Build the path
                // (inline root -> run node), keep the shared prefix already on the stack,
                // pop the rest, and resolve styles only for the newly-descended nodes.
                path_scratch.clear();
                let mut walk_id = Some(style.brush.id);
                while let Some(node_id) = walk_id {
                    path_scratch.push(node_id);
                    if node_id == inline_root_id {
                        break;
                    }
                    walk_id = doc.get_node(node_id).and_then(|node| node.parent);
                }
                path_scratch.reverse();

                let shared = stack
                    .iter()
                    .zip(path_scratch.iter())
                    .take_while(|(entry, node_id)| entry.node_id == **node_id)
                    .count();
                stack.truncate(shared);
                for &node_id in &path_scratch[shared..] {
                    stack.push(resolve_decoration_entry(doc, node_id));
                }

                // The glyph colour comes from the run's own node (the stack top): `color`
                // inherits, so the innermost inline element already carries the right value.
                let text_color = color
                    .or(stack.last().map(|e| e.text_color))
                    .unwrap_or(Color::BLACK);

                let embolden = embolden(font_size, scale, run.synthesis().embolden());

                // (limit, where "…" starts) when this run is where the line gets cut
                let cut_here = cut
                    .filter(|limit| glyph_run.offset() + glyph_run.advance() > *limit)
                    .map(|limit| (limit, kept_end(&glyph_run, limit)));
                if cut_here.is_some() {
                    cut = Some(f32::NAN);
                }

                let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
                scene.draw_glyphs(
                    font,
                    font_size,
                    !FONT_EMBOLDEN_ENABLED, // hint
                    normalized_coords,
                    embolden,
                    Fill::NonZero,
                    &acde_gpu::Paint::from(text_color),
                    1.0, // alpha
                    transform,
                    glyph_xform,
                    glyph_run
                        .positioned_glyphs()
                        .take_while(|g| cut_here.is_none_or(|(limit, _)| g.x + g.advance <= limit))
                        .map(|glyph| acde_gpu::Glyph {
                            id: glyph.id as _,
                            x: glyph.x,
                            y: glyph.y,
                        }),
                );
                // "…" in the box's own font and colour
                if let (Some((_, x)), Some((_, marker))) = (cut_here, &ellipsis) {
                    let root_color = stack.first().map_or(text_color, |e| e.text_color);
                    draw_ellipsis_marker(
                        scene,
                        marker,
                        (x, glyph_run.baseline()),
                        root_color,
                        transform,
                        scale,
                    );
                }

                // Accumulate this run's contribution to each decorating box on its ancestor
                // path. The decoration is drawn once per box after the whole line has been
                // walked (see `flush_line_decorations`), so mixed font sizes within a box
                // produce a single straight line rather than one stepped segment per run.
                // acrender: only when some box on the path has a decoration
                if stack.iter().all(|entry| entry.decoration.is_none()) {
                    continue;
                }
                let geometry = DecorationRunGeometry {
                    baseline: glyph_run.baseline(),
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    underline_offset: metrics.underline_offset,
                    underline_size: metrics.underline_size,
                    strikethrough_size: metrics.strikethrough_size,
                    font: font.clone(),
                    font_size,
                    css_font_size,
                };
                let run_node_id = style.brush.id;
                let run_x0 = glyph_run.offset() as f64;
                let run_x1 = match (cut_here, &ellipsis) {
                    (Some((_, x)), Some((_, marker))) => (x + marker.advance()) as f64,
                    _ => run_x0 + glyph_run.advance() as f64,
                };

                for entry in stack.iter() {
                    if entry.decoration.is_none() {
                        continue;
                    }
                    let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                        Some(idx) => idx,
                        None => {
                            deco_boxes.push(LineDecoration {
                                node_id: entry.node_id,
                                deco: entry.decoration.clone().unwrap(),
                                min_x: f64::INFINITY,
                                max_x: f64::NEG_INFINITY,
                                own: None,
                                first: None,
                            });
                            deco_boxes.len() - 1
                        }
                    };
                    let acc = &mut deco_boxes[idx];
                    acc.min_x = acc.min_x.min(run_x0);
                    acc.max_x = acc.max_x.max(run_x1);
                    if acc.first.is_none() {
                        acc.first = Some(geometry.clone());
                    }
                    // A run whose innermost node is the box itself is the box's own text, so
                    // its font is the one Firefox positions and sizes the decoration with.
                    if entry.node_id == run_node_id {
                        acc.own = Some(geometry.clone());
                    }
                }
            }
        }

        // acrender: a line `-webkit-line-clamp` ends on that fits whole: "…" goes after it
        if let (true, Some(limit), Some((_, marker))) = (clamped, cut, &ellipsis)
            && !limit.is_nan()
        {
            let color = stack.first().map_or(Color::BLACK, |e| e.text_color);
            let at = (line_end, line.metrics().baseline);
            draw_ellipsis_marker(scene, marker, at, color, transform, scale);
        }

        flush_line_decorations(
            scene,
            transform,
            scale,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            line.metrics().baseline,
        );
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
