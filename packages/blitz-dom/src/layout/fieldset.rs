//! acrender: the rendering of `<fieldset>` and its rendered `<legend>`
//! (<https://html.spec.whatwg.org/multipage/rendering.html#the-fieldset-and-legend-elements>).
//!
//! The rendered legend is the fieldset's first child that is a `legend` element, not floated and not
//! absolutely positioned. It is a block-level box whatever its `display`, takes no part in the
//! fieldset's own formatting context (block, flex or grid), shrinks to fit the fieldset's content box
//! in the inline axis and sits over the block-start border, centred on it. The space the border takes
//! at the block start is the larger of its width and the legend's margin box, so the content starts
//! below the legend; the border itself is painted through the legend's middle, with a gap behind it
//! (blitz-paint).
//!
//! Instead of an anonymous fieldset content box, the fieldset's own algorithm lays out its content
//! with the legend left out of its children and the block-start border widened to the legend band
//! ([`stylo_taffy::LegendBand`]): the geometry is the same, as the spec moves the fieldset's padding
//! onto that box and gives it the fieldset's display.
//!
//! Not done: vertical writing modes (the legend is laid out as ordinary content there), the legend
//! staying out of the fieldset's overflow clip, and the legacy `align` attribute.

use markup5ever::local_name;
use style::computed_values::float::T as Float;
use style::computed_values::position::T as Position;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use stylo_taffy::LegendBand;
use taffy::{
    AvailableSpace, CoreStyle as _, Layout, LayoutInput, LayoutOutput, LayoutPartialTree as _,
    Line, MaybeMath as _, MaybeResolve as _, NodeId, Point, Rect, RequestedAxis,
    ResolveOrZero as _, RunMode, Size, SizingMode,
};

use super::{LayoutPassState, resolve_calc_value};
use crate::{BaseDocument, dom_node_id, node::Node, taffy_node_id};

/// A fieldset whose content is being laid out without its rendered legend.
#[derive(Copy, Clone)]
pub(crate) struct FieldsetFrame {
    pub(crate) fieldset: crate::NodeId,
    pub(crate) legend: crate::NodeId,
    pub(crate) band: LegendBand,
}

fn is_html(node: &Node, name: markup5ever::LocalName) -> bool {
    node.element_data()
        .is_some_and(|el| el.name.local == name && el.name.ns == markup5ever::ns!(html))
}

/// The child of `fieldset` that would be its rendered legend, from the DOM (before box construction:
/// box construction keeps it block-level and a direct layout child).
pub(crate) fn legend_candidate(
    doc: &BaseDocument,
    fieldset: crate::NodeId,
) -> Option<crate::NodeId> {
    let node = &doc.nodes[fieldset];
    if !is_html(node, local_name!("fieldset")) {
        return None;
    }
    node.children.iter().copied().find(|&id| {
        let child = &doc.nodes[id];
        if !is_html(child, local_name!("legend")) {
            return false;
        }
        let Some(style) = child.primary_styles() else {
            return false;
        };
        let display = style.clone_display();
        !matches!(
            display.inside(),
            DisplayInside::None | DisplayInside::Contents
        ) && style.clone_float() == Float::None
            && !matches!(style.clone_position(), Position::Absolute | Position::Fixed)
    })
}

impl BaseDocument {
    /// The rendered legend of `fieldset`, when it is a fieldset box (block, flex or grid) that has one
    pub fn rendered_legend(&self, fieldset: crate::NodeId) -> Option<crate::NodeId> {
        let node = self.get_node(fieldset)?;
        let display = node.primary_styles()?.clone_display();
        if !matches!(
            display.inside(),
            DisplayInside::Flow
                | DisplayInside::FlowRoot
                | DisplayInside::Flex
                | DisplayInside::Grid
        ) || (display.inside() == DisplayInside::Flow
            && display.outside() == DisplayOutside::Inline)
            || node.flags.is_inline_root()
            || node.flags.is_table_root()
        {
            return None;
        }
        #[cfg(feature = "writing-mode")]
        if !node.writing_mode().is_horizontal() {
            return None;
        }
        let legend = legend_candidate(self, fieldset)?;
        node.layout_children
            .borrow()
            .as_ref()?
            .contains(&legend)
            .then_some(legend)
    }
}

impl LayoutPassState<'_> {
    /// The frame of `node` while its content is laid out around its rendered legend
    pub(crate) fn fieldset_frame(&self, node: crate::NodeId) -> Option<FieldsetFrame> {
        self.fieldsets
            .iter()
            .rev()
            .find(|f| f.fieldset == node)
            .copied()
    }

    /// Lays out a fieldset that has a rendered legend: the legend first (its size decides the
    /// band), then the content with the fieldset's algorithm, then the legend's place.
    pub(crate) fn compute_fieldset_layout(
        &mut self,
        node_id: NodeId,
        legend: crate::NodeId,
        inputs: LayoutInput,
        algorithm: fn(&mut Self, NodeId, LayoutInput) -> LayoutOutput,
    ) -> LayoutOutput {
        let fieldset = dom_node_id(node_id);
        let parent_width = inputs.parent_size.width;
        let (border, padding) = {
            let style = self.nodes[fieldset].layout_style();
            (
                style
                    .border()
                    .resolve_or_zero(parent_width, resolve_calc_value),
                style
                    .padding()
                    .resolve_or_zero(parent_width, resolve_calc_value),
            )
        };
        let inline_inset = border.left + border.right + padding.left + padding.right;

        let content_width = inputs
            .known_dimensions
            .width
            .map(|w| (w - inline_inset).max(0.0));
        let available = match (content_width, inputs.available_space.width) {
            (Some(width), _) => AvailableSpace::Definite(width),
            (None, AvailableSpace::Definite(width)) => {
                AvailableSpace::Definite((width - inline_inset).max(0.0))
            }
            (None, other) => other,
        };
        let (margin, inset) = {
            let style = self.child_layout_style(&self.nodes[legend]);
            let basis = content_width.or(available.into_option());
            (
                style
                    .margin()
                    .map(|m| m.maybe_resolve(basis, resolve_calc_value)),
                style
                    .inset()
                    .map(|i| i.maybe_resolve(basis, resolve_calc_value)),
            )
        };
        let margin_sum = |a: Option<f32>, b: Option<f32>| a.unwrap_or(0.0) + b.unwrap_or(0.0);
        let measure = LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Both,
            known_dimensions: Size::NONE,
            known_dimensions_are_definite: Size {
                width: true,
                height: true,
            },
            parent_size: Size {
                width: content_width.or(available.into_option()),
                height: None,
            },
            available_space: Size {
                width: available.maybe_sub(margin_sum(margin.left, margin.right)),
                height: AvailableSpace::MaxContent,
            },
            vertical_margins_are_collapsible: Line::FALSE,
        };
        let size = self
            .compute_child_layout(taffy_node_id(legend), measure)
            .size;
        let (margin_top, margin_bottom) = (margin.top.unwrap_or(0.0), margin.bottom.unwrap_or(0.0));
        let margin_box = margin_top + size.height + margin_bottom;
        let band = LegendBand {
            border: border.top.max(margin_box),
            spill: (margin_box - border.top).max(0.0),
        };

        self.fieldsets.push(FieldsetFrame {
            fieldset,
            legend,
            band,
        });
        let mut output = algorithm(self, node_id, inputs);
        self.fieldsets.pop();

        if inputs.known_dimensions.width.is_none() {
            let legend_width = size.width + margin_sum(margin.left, margin.right) + inline_inset;
            output.size.width = output.size.width.max(legend_width);
        }
        if inputs.run_mode != RunMode::PerformLayout {
            return output;
        }

        let content_width = (output.size.width - inline_inset).max(0.0);
        let free = content_width - size.width - margin_sum(margin.left, margin.right);
        let x = border.left
            + padding.left
            + match (margin.left, margin.right) {
                (None, None) => free.max(0.0) / 2.0,
                (None, Some(right)) => free.max(0.0) + right,
                (Some(left), _) => left,
            };
        let centre = match margin_box >= border.top {
            true => margin_top + size.height / 2.0,
            false => border.top / 2.0,
        };
        let relative = self.nodes[legend]
            .primary_styles()
            .is_some_and(|s| s.clone_position() == Position::Relative);
        let shift = match relative {
            true => Point {
                x: inset.left.or(inset.right.map(|r| -r)).unwrap_or(0.0),
                y: inset.top.or(inset.bottom.map(|b| -b)).unwrap_or(0.0),
            },
            false => Point::ZERO,
        };
        let location = Point {
            x: x + shift.x,
            y: centre - size.height / 2.0 + shift.y,
        };

        let perform = LayoutInput {
            run_mode: RunMode::PerformLayout,
            known_dimensions: size.map(Some),
            parent_size: Size {
                width: Some(content_width),
                height: None,
            },
            available_space: Size {
                width: AvailableSpace::Definite(size.width),
                height: AvailableSpace::Definite(size.height),
            },
            ..measure
        };
        let mut placed = self.compute_child_layout(taffy_node_id(legend), perform);
        let (legend_padding, legend_border) = {
            let style = self.child_layout_style(&self.nodes[legend]);
            (
                style
                    .padding()
                    .resolve_or_zero(Some(content_width), resolve_calc_value),
                style
                    .border()
                    .resolve_or_zero(Some(content_width), resolve_calc_value),
            )
        };
        let layout = Layout {
            order: 0,
            location,
            size: placed.size,
            scrollable_overflow_rect: placed.scrollable_overflow_rect,
            scrollbar_size: Size::ZERO,
            border: legend_border,
            padding: legend_padding,
            margin: Rect {
                left: margin
                    .left
                    .unwrap_or(location.x - shift.x - border.left - padding.left),
                right: margin.right.unwrap_or(0.0),
                top: margin_top,
                bottom: margin_bottom,
            },
        };
        self.set_unrounded_layout(taffy_node_id(legend), &layout);
        if !placed.oof_candidates.is_empty() {
            placed.oof_candidates.translate(location);
            output.oof_candidates.append(&mut placed.oof_candidates);
        }
        output
    }
}
