//! Enable the dom to lay itself out using taffy
//!
//! In servo, style and layout happen together during traversal
//! However, in Blitz, we do a style pass then a layout pass.
//! This is slower, yes, but happens fast enough that it's not a huge issue.

use crate::node::{ComputedStyleRef, ImageData, NodeData, SpecialElementData};
use crate::{document::BaseDocument, dom_node_id, node::Node, taffy_node_id};
use markup5ever::{LocalName, local_name};
use std::cell::Ref;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use style::Atom;
use style::values::computed::CSSPixelLength;
use style::values::computed::length_percentage::CalcLengthPercentage;
use stylo_taffy::TaffyStyloStyle;
use taffy::{
    AxisStaticEdge, AxisStaticPosition, BlockContext, CoreStyle as _, DetailedLayoutInfo,
    LayoutContainingBlock, LayoutPartialTree, MaybeMath as _, NodeId, OofCandidate, ResolveOrZero,
    RoundTree, RunMode, TraversePartialTree, TraverseTree, compute_block_layout,
    compute_cached_layout, compute_flexbox_layout, compute_grid_layout, compute_leaf_layout,
    compute_oof_layout, prelude::*,
};

pub(crate) mod construct;
pub(crate) mod damage;
pub(crate) mod fieldset;
pub(crate) mod inline;
pub(crate) mod list;
pub(crate) mod paint_tree;
pub(crate) mod replaced;
pub mod select; // acrender
pub(crate) mod table;
pub(crate) mod text_transform;
#[cfg(feature = "writing-mode")]
pub(crate) mod writing_mode;

use self::replaced::{
    IntrinsicSizes, ReplacedContext, compute_replaced_layout, is_replaced_element,
};
use self::table::TableTreeWrapper;

/// The default object size for replaced elements
/// (https://drafts.csswg.org/css-images/#default-object-size).
const DEFAULT_OBJECT_SIZE: taffy::Size<f32> = taffy::Size {
    width: 300.0,
    height: 150.0,
};

/// The intrinsic dimensions and default object size for a replaced element
/// whose intrinsic dimensions are determined by its tag: an image with no
/// loaded resource has no intrinsic dimensions and a zero default object size;
/// a canvas has an intrinsic size and aspect ratio given by its width/height
/// attributes (defaulting to 300x150); other replaced elements (video, iframe,
/// embed) have no intrinsic dimensions and the 300x150 default object size.
fn tag_intrinsic_sizes(
    tag_name: &LocalName,
    attr_size: taffy::Size<Option<f32>>,
) -> (IntrinsicSizes, taffy::Size<f32>) {
    if *tag_name == local_name!("img") || *tag_name == local_name!("svg") {
        return (IntrinsicSizes::default(), taffy::Size::ZERO);
    }
    if *tag_name == local_name!("canvas") {
        let width = attr_size.width.unwrap_or(300.0);
        let height = attr_size.height.unwrap_or(150.0);
        return (
            IntrinsicSizes {
                width: Some(width),
                height: Some(height),
                ratio: Some(width / height),
            },
            DEFAULT_OBJECT_SIZE,
        );
    }
    (IntrinsicSizes::default(), DEFAULT_OBJECT_SIZE)
}

pub(crate) fn resolve_calc_value(calc_ptr: *const (), parent_size: f32) -> f32 {
    let calc = unsafe { &*(calc_ptr as *const CalcLengthPercentage) };
    let result = calc.resolve(CSSPixelLength::new(parent_size));
    result.px()
}

/// Per-pass layout state: wraps the document for the duration of a layout pass and carries
/// the Taffy tree trait implementations. Derefs to [`BaseDocument`].
pub(crate) struct LayoutPassState<'doc> {
    doc: &'doc mut BaseDocument,
    /// The writing mode of the box whose layout algorithm is currently running (see
    /// `layout::writing_mode`). Child styles read during that algorithm are expressed in its axes.
    #[cfg(feature = "writing-mode")]
    pub(crate) layout_wm: stylo_taffy::WritingMode,
    /// The root element and the writing mode its algorithm runs in (see `BaseDocument::root_layout_wm`).
    #[cfg(feature = "writing-mode")]
    root_id: Option<crate::NodeId>,
    #[cfg(feature = "writing-mode")]
    root_wm: stylo_taffy::WritingMode,
    /// Whether the node whose layout algorithm is running aligns its children's `align-self` in its
    /// inline axis (a column flex container).
    #[cfg(feature = "writing-mode")]
    pub(crate) current_align_axis_is_inline: bool,
    /// The box currently being laid out in an orthogonal flow and its containing block's inline
    /// size, against which its percentage padding resolves (see `layout::writing_mode`).
    #[cfg(feature = "writing-mode")]
    pub(crate) orthogonal_percent_basis: Option<(crate::NodeId, f32)>,
    /// acrender: fieldsets whose content is being laid out around their rendered legend
    /// (see `layout::fieldset`)
    pub(crate) fieldsets: Vec<fieldset::FieldsetFrame>,
}

impl<'doc> LayoutPassState<'doc> {
    /// The containing block's inline size when `dom_id` is the box currently laid out in an orthogonal
    /// flow: its percentage padding resolves against it (see `layout::writing_mode`).
    #[inline]
    fn orthogonal_percent_basis_of(&self, dom_id: crate::NodeId) -> Option<f32> {
        #[cfg(feature = "writing-mode")]
        {
            self.orthogonal_percent_basis
                .filter(|(node, _)| *node == dom_id)
                .map(|(_, basis)| basis)
        }
        #[cfg(not(feature = "writing-mode"))]
        {
            let _ = dom_id;
            None
        }
    }

    pub(crate) fn new(doc: &'doc mut BaseDocument) -> Self {
        #[cfg(feature = "writing-mode")]
        let root_id = doc.try_root_element().map(|root| root.id);
        Self {
            #[cfg(feature = "writing-mode")]
            root_wm: root_id.map_or(stylo_taffy::WritingMode::empty(), |id| {
                doc.root_layout_wm(id)
            }),
            doc,
            #[cfg(feature = "writing-mode")]
            root_id,
            #[cfg(feature = "writing-mode")]
            layout_wm: stylo_taffy::WritingMode::empty(),
            #[cfg(feature = "writing-mode")]
            current_align_axis_is_inline: false,
            #[cfg(feature = "writing-mode")]
            orthogonal_percent_basis: None,
            fieldsets: Vec::new(),
        }
    }
}

impl Deref for LayoutPassState<'_> {
    type Target = BaseDocument;
    fn deref(&self) -> &BaseDocument {
        self.doc
    }
}

impl DerefMut for LayoutPassState<'_> {
    fn deref_mut(&mut self) -> &mut BaseDocument {
        self.doc
    }
}

impl LayoutPassState<'_> {
    fn node_from_id(&self, node_id: taffy::prelude::NodeId) -> &Node {
        &self.nodes[dom_node_id(node_id)]
    }
    fn node_from_id_mut(&mut self, node_id: taffy::prelude::NodeId) -> &mut Node {
        &mut self.nodes[dom_node_id(node_id)]
    }
}

impl LayoutPassState<'_> {
    /// Run the node's layout algorithm, then lay out the out-of-flow (absolute/fixed)
    /// boxes for which it is the containing block. Must be called inside the layout
    /// cache wrapper so that cache hits do not re-run the out-of-flow pass.
    fn compute_child_layout_internal(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::tree::LayoutOutput {
        #[cfg(feature = "writing-mode")]
        {
            self.compute_child_layout_in_own_wm(node_id, inputs, block_ctx)
        }
        #[cfg(not(feature = "writing-mode"))]
        {
            self.compute_child_layout_in_current_wm(node_id, inputs, block_ctx)
        }
    }

    /// Lay out `node_id` in the writing mode of the currently running algorithm (its parent's)
    pub(crate) fn compute_child_layout_in_current_wm(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::tree::LayoutOutput {
        let mut output = self.dispatch_child_layout(node_id, inputs, block_ctx);
        if inputs.run_mode == RunMode::PerformLayout {
            compute_oof_layout(self, node_id, &mut output);
        }
        output
    }

    /// The style of a node being laid out *by* the node whose algorithm is currently running,
    /// expressed in that node's writing mode (see `layout::writing_mode`).
    #[inline]
    pub(crate) fn child_layout_style<'a>(
        &self,
        node: &'a Node,
    ) -> stylo_taffy::TaffyStyloStyle<ComputedStyleRef<'a>> {
        #[cfg(feature = "writing-mode")]
        {
            let mut style = node.layout_style_in(self.layout_wm);
            style.align_axis_is_inline = self.current_align_axis_is_inline;
            style
        }
        #[cfg(not(feature = "writing-mode"))]
        {
            node.layout_style()
        }
    }

    fn dispatch_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::tree::LayoutOutput {
        let orthogonal_percent_basis = self.orthogonal_percent_basis_of(dom_node_id(node_id));
        let video_size = self.video_sizes.get(&dom_node_id(node_id)).copied(); // acrender
        let node = &mut self.nodes[dom_node_id(node_id)];
        let native_appearance = node.has_native_appearance(); // acrender
        let content_sized = field_sized(node); // acrender

        let font_styles = node.primary_styles().map(|style| {
            use style::values::computed::font::LineHeight;

            let font_size = style.clone_font_size().used_size().px();
            let line_height = match style.clone_line_height() {
                LineHeight::Normal => font_size * 1.2,
                LineHeight::Number(num) => font_size * num.0,
                LineHeight::Length(value) => value.0.px(),
            };

            (font_size, line_height)
        });
        let font_size = font_styles.map(|s| s.0);
        let resolved_line_height = font_styles.map(|s| s.1);

        match &mut node.data {
            NodeData::Text(data) => {
                // With the new "inline context" architecture all text nodes should be wrapped in an "inline layout context"
                // and should therefore never be measured individually.
                #[cfg(feature = "tracing")]
                tracing::error!(
                    node_id = ?dom_node_id(node_id),
                    data = ?data,
                    "Tried to lay out text node individually",
                );

                #[cfg(not(feature = "tracing"))]
                let _ = data;

                taffy::LayoutOutput::HIDDEN
                // unreachable!();

                // compute_leaf_layout(inputs, &node.style, |known_dimensions, available_space| {
                //     let context = TextContext {
                //         text_content: &data.content.trim(),
                //         writing_mode: WritingMode::Horizontal,
                //     };
                //     let font_metrics = FontMetrics {
                //         char_width: 8.0,
                //         char_height: 16.0,
                //     };
                //     text_measure_function(
                //         known_dimensions,
                //         available_space,
                //         &context,
                //         &font_metrics,
                //     )
                // })
            }
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // TODO: deduplicate with single-line text input
                if *element_data.name.local == *"textarea" {
                    let rows = element_data
                        .attr(local_name!("rows"))
                        .and_then(|val| val.parse::<f32>().ok())
                        .unwrap_or(2.0);

                    let cols = element_data
                        .attr(local_name!("cols"))
                        .and_then(|val| val.parse::<f32>().ok());

                    let line_height = resolved_line_height.unwrap_or(16.0);
                    // acrender: `field-sizing: content` sizes the box to its text (rows and cols ignored)
                    let wrap = content_sized.then(|| text_field_wrap(node, inputs));
                    let content = wrap.map(|wrap| {
                        measure_text_field(self.doc, dom_node_id(node_id), wrap)
                            .map(|(width, height)| (width, height.max(line_height)))
                            .unwrap_or((0.0, line_height))
                    });
                    let node = &self.doc.nodes[dom_node_id(node_id)];
                    let style = node.layout_style();
                    let mut output = compute_leaf_layout(
                        inputs,
                        &style,
                        resolve_calc_value,
                        |_known_size, _available_space| match content {
                            Some((width, height)) => taffy::Size { width, height },
                            None => taffy::Size {
                                width: cols
                                    .map(|cols| cols * font_size.unwrap_or(16.0) * 0.6)
                                    .unwrap_or(300.0),
                                height: line_height * rows,
                            },
                        },
                    );
                    if inputs.run_mode == RunMode::PerformLayout {
                        let padding = style
                            .padding()
                            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                        let pb = padding
                            + style
                                .border()
                                .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                        let content_width = (output.size.width - pb.horizontal_axis_sum()).max(0.0);
                        let doc = &mut *self.doc;
                        let scale = doc.viewport.scale();
                        let node = &mut doc.nodes[dom_node_id(node_id)];
                        if let Some(input) = node
                            .data
                            .downcast_element_mut()
                            .and_then(|el| el.text_input_data_mut())
                        {
                            input.editor.set_width(Some(content_width * scale));
                            let font_ctx = &mut doc.font_ctx.lock().unwrap();
                            input.editor.refresh_layout(font_ctx, &mut doc.layout_ctx);
                            if let Some(placeholder) = input.placeholder.as_mut() {
                                placeholder.set_width(Some(content_width * scale));
                                placeholder.refresh_layout(font_ctx, &mut doc.layout_ctx);
                            }
                            // acrender: the text scrolls inside the box, so scrollHeight covers all of it
                            // (chat composers grow the box to its scrollHeight)
                            if let Some(text) = input.editor.try_layout() {
                                output.scrollable_overflow_rect.bottom = output
                                    .scrollable_overflow_rect
                                    .bottom
                                    .max(padding.top + text.height() / scale + padding.bottom);
                            }
                        }
                    }
                    return output;
                }

                // acrender: a select is a leaf sized by its rows (layout::select); a list box scrolls
                // through all of them
                if let Some(select) = element_data.select_data() {
                    let (width, height) = select.content_size();
                    let rows = select
                        .list_box
                        .then_some(select.row_height * select.items.len() as f32);
                    let style = node.layout_style();
                    let mut output = compute_leaf_layout(
                        inputs,
                        &style,
                        resolve_calc_value,
                        |_known_size, _available_space| taffy::Size { width, height },
                    );
                    if let Some(rows) = rows {
                        let padding = style
                            .padding()
                            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                        output.scrollable_overflow_rect.bottom = output
                            .scrollable_overflow_rect
                            .bottom
                            .max(padding.top + rows + padding.bottom);
                    }
                    return output;
                }

                if *element_data.name.local == *"input" {
                    match element_data.attr(local_name!("type")) {
                        // if the input type is hidden, hide it
                        Some("hidden") => {
                            return taffy::LayoutOutput::HIDDEN;
                        }
                        Some("checkbox") if native_appearance => {
                            return compute_leaf_layout(
                                inputs,
                                &node.layout_style(),
                                resolve_calc_value,
                                |_known_size, _available_space| {
                                    let size = node.layout_style().size();
                                    let width = size.width.resolve_or_zero(
                                        inputs.parent_size.width,
                                        resolve_calc_value,
                                    );
                                    let height = size.height.resolve_or_zero(
                                        inputs.parent_size.height,
                                        resolve_calc_value,
                                    );
                                    let min_size = width.min(height);
                                    taffy::Size {
                                        width: min_size,
                                        height: min_size,
                                    }
                                },
                            );
                        }
                        None | Some("text" | "password" | "email" | "tel" | "url" | "search")
                            if content_sized =>
                        {
                            // acrender: `field-sizing: content`: as wide as its text, plus the caret
                            let line_height = resolved_line_height.unwrap_or(16.0);
                            let (width, _) =
                                measure_text_field(self.doc, dom_node_id(node_id), None).unwrap_or((0.0, 0.0));
                            let node = &self.doc.nodes[dom_node_id(node_id)];
                            return compute_leaf_layout(
                                inputs,
                                &node.layout_style(),
                                resolve_calc_value,
                                |_known_size, _available_space| taffy::Size {
                                    width: width + 1.0,
                                    height: line_height,
                                },
                            );
                        }
                        None | Some("text" | "password" | "email" | "tel" | "url" | "search") => {
                            return compute_leaf_layout(
                                inputs,
                                &node.layout_style(),
                                resolve_calc_value,
                                |_known_size, _available_space| taffy::Size {
                                    width: match inputs.available_space.width {
                                        AvailableSpace::Definite(limit) => limit.min(300.0),
                                        AvailableSpace::MinContent => 0.0,
                                        AvailableSpace::MaxContent => 300.0,
                                    },
                                    height: resolved_line_height.unwrap_or(16.0),
                                },
                            );
                        }
                        _ => {}
                    }
                }

                if is_replaced_element(&element_data.name.local) {
                    // Width/height attributes are presentational hints mapped to the CSS
                    // width/height properties by
                    // `synthesize_presentational_hints_for_legacy_attributes`, so they
                    // are already part of the style. They are only read here for the
                    // elements whose attributes determine their *intrinsic* size
                    // (canvas, and custom widgets on canvas tags).
                    let attr_size = taffy::Size {
                        width: element_data
                            .attr(local_name!("width"))
                            .and_then(|val| val.parse::<f32>().ok()),
                        height: element_data
                            .attr(local_name!("height"))
                            .and_then(|val| val.parse::<f32>().ok()),
                    };

                    // Get the element's intrinsic dimensions and default object size
                    let (intrinsic_sizes, default_object_size) = match &element_data.special_data {
                        SpecialElementData::Image(image_data) => match &**image_data {
                            ImageData::Raster(image) => {
                                let (width, height) = (image.width as f32, image.height as f32);
                                (
                                    IntrinsicSizes {
                                        width: Some(width),
                                        height: Some(height),
                                        ratio: Some(width / height),
                                    },
                                    DEFAULT_OBJECT_SIZE,
                                )
                            }
                            #[cfg(feature = "svg")]
                            ImageData::Svg(svg) => {
                                let mut width = svg.intrinsic_width();
                                let mut height = svg.intrinsic_height();
                                // An SVG with no declared dimensions and no viewBox has no
                                // intrinsic dimensions per CSS, but usvg still resolves a
                                // concrete size; use it in place of the default object size.
                                if width.is_none()
                                    && height.is_none()
                                    && svg.viewbox_aspect_ratio().is_none()
                                {
                                    let size = svg.tree.size();
                                    width = Some(size.width());
                                    height = Some(size.height());
                                }
                                (
                                    IntrinsicSizes {
                                        width,
                                        height,
                                        ratio: Some(svg.aspect_ratio()),
                                    },
                                    DEFAULT_OBJECT_SIZE,
                                )
                            }
                            ImageData::None => (IntrinsicSizes::default(), taffy::Size::ZERO),
                        },
                        SpecialElementData::SubDocument(_) | SpecialElementData::None => {
                            // acrender: a <video> with a frame is as big as its frames
                            match video_size {
                                Some((width, height)) if element_data.name.local == local_name!("video") && width > 0.0 && height > 0.0 => (
                                    IntrinsicSizes { width: Some(width), height: Some(height), ratio: Some(width / height) },
                                    DEFAULT_OBJECT_SIZE,
                                ),
                                _ => tag_intrinsic_sizes(&element_data.name.local, attr_size),
                            }
                        }
                        _ => unreachable!(),
                    };

                    #[cfg(feature = "writing-mode")]
                    let (intrinsic_sizes, default_object_size) =
                        if node.writing_mode().is_vertical() {
                            // Intrinsic sizes are physical; the element's algorithm runs in its own writing mode
                            (
                                crate::layout::replaced::IntrinsicSizes {
                                    width: intrinsic_sizes.height,
                                    height: intrinsic_sizes.width,
                                    ratio: intrinsic_sizes.ratio.map(|ratio| 1.0 / ratio),
                                },
                                default_object_size.transpose(),
                            )
                        } else {
                            (intrinsic_sizes, default_object_size)
                        };

                    let replaced_context = ReplacedContext {
                        intrinsic_sizes,
                        default_object_size,
                    };

                    let mut style = node.layout_style();
                    style.set_percent_basis(orthogonal_percent_basis);
                    return compute_replaced_layout(
                        inputs,
                        &style,
                        resolve_calc_value,
                        &replaced_context,
                    );
                }

                if node.flags.is_table_root() {
                    // The space available to an absolutely positioned table never exceeds
                    // the size of its containing block (less the table's margins), even
                    // when negative insets would otherwise add to it
                    // (https://drafts.csswg.org/css-tables-3/#abspos).
                    let mut inputs = inputs;
                    if self.nodes[dom_node_id(node_id)].is_out_of_flow() {
                        let margin = self.nodes[dom_node_id(node_id)]
                            .layout_style()
                            .margin()
                            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
                        let max_available_space = inputs
                            .parent_size
                            .maybe_sub(margin.sum_axes())
                            .maybe_max(taffy::Size::ZERO);
                        inputs.available_space =
                            inputs.available_space.maybe_min(max_available_space);
                    }

                    let SpecialElementData::TableRoot(context) = &self.nodes[dom_node_id(node_id)]
                        .data
                        .downcast_element()
                        .unwrap()
                        .special_data
                    else {
                        panic!("Node marked as table root but doesn't have TableContext");
                    };
                    let context = Arc::clone(context);

                    let mut table_wrapper = TableTreeWrapper {
                        doc: self,
                        ctx: context,
                    };
                    let mut output = compute_grid_layout(&mut table_wrapper, node_id, inputs);

                    // Absolutely positioned children take no part in the table grid. Their
                    // static position is the top-left of the table's content box.
                    let context = table_wrapper.ctx;
                    if inputs.run_mode == RunMode::PerformLayout && !context.oof_children.is_empty()
                    {
                        let style = &context.style;
                        let parent_width = inputs.parent_size.width;
                        let padding = style
                            .padding
                            .resolve_or_zero(parent_width, resolve_calc_value);
                        let border = style
                            .border
                            .resolve_or_zero(parent_width, resolve_calc_value);
                        let static_position = taffy::Point {
                            x: padding.left + border.left,
                            y: padding.top + border.top,
                        }
                        .map(|pos| AxisStaticPosition::from_edge(pos, AxisStaticEdge::Start));
                        let in_flow_count = context.cells.len();
                        for (idx, (child_id, position)) in context.oof_children.iter().enumerate() {
                            output.oof_candidates.push(OofCandidate {
                                node: taffy_node_id(*child_id),
                                order: (in_flow_count + idx) as u32,
                                position: *position,
                                static_position,
                            });
                        }
                    }

                    // HACK: Cap scrollable overflow at node size to prevent scrolling
                    output.scrollable_overflow_rect.left = 0.0;
                    output.scrollable_overflow_rect.top = 0.0;
                    output.scrollable_overflow_rect.right =
                        output.scrollable_overflow_rect.right.min(output.size.width);
                    output.scrollable_overflow_rect.bottom = output
                        .scrollable_overflow_rect
                        .bottom
                        .min(output.size.height);

                    return output;
                }

                if node.flags.is_inline_root() {
                    return self.compute_inline_layout(dom_node_id(node_id), inputs, block_ctx);
                }

                // acrender: a fieldset lays out its content around its rendered legend
                let display = node.taffy_display();
                #[cfg(feature = "writing-mode")]
                let column_flex = node.is_column_flex_container();
                if let Some(legend) = self.rendered_legend(dom_node_id(node_id)) {
                    let algorithm: fn(
                        &mut Self,
                        NodeId,
                        taffy::LayoutInput,
                    ) -> taffy::LayoutOutput = match display {
                        Display::Flex => {
                            #[cfg(feature = "writing-mode")]
                            {
                                self.current_align_axis_is_inline = column_flex;
                            }
                            |tree, id, inputs| compute_flexbox_layout(tree, id, inputs)
                        }
                        Display::Grid => |tree, id, inputs| compute_grid_layout(tree, id, inputs),
                        _ => |tree, id, inputs| compute_block_layout(tree, id, inputs, None),
                    };
                    return self.compute_fieldset_layout(node_id, legend, inputs, algorithm);
                }

                // The default CSS file will set
                match display {
                    Display::Block => compute_block_layout(self, node_id, inputs, block_ctx),
                    Display::FlowRoot => compute_block_layout(self, node_id, inputs, None),
                    Display::Flex => {
                        #[cfg(feature = "writing-mode")]
                        {
                            self.current_align_axis_is_inline = column_flex;
                        }
                        compute_flexbox_layout(self, node_id, inputs)
                    }
                    Display::Grid => compute_grid_layout(self, node_id, inputs),
                    Display::None => taffy::LayoutOutput::HIDDEN,
                }
            }
            NodeData::Document(_) => compute_block_layout(self, node_id, inputs, None),

            _ => taffy::LayoutOutput::HIDDEN,
        }
    }
}

impl TraversePartialTree for LayoutPassState<'_> {
    type ChildIter<'a>
        = RefCellChildIter<'a>
    where
        Self: 'a;

    fn child_ids(&self, node_id: NodeId) -> Self::ChildIter<'_> {
        let layout_children = self.node_from_id(node_id).layout_children.borrow(); //.unwrap().as_ref();
        let mut iter = RefCellChildIter::new(Ref::map(layout_children, |children| {
            children.as_ref().map(|c| c.as_slice()).unwrap_or(&[])
        }));
        // acrender: the rendered legend is not one of the fieldset's content's children
        iter.skip = self
            .fieldset_frame(dom_node_id(node_id))
            .map(|frame| frame.legend);
        iter
    }

    fn child_count(&self, node_id: NodeId) -> usize {
        let count = self
            .node_from_id(node_id)
            .layout_children
            .borrow()
            .as_ref()
            .map(|c| c.len())
            .unwrap_or(0);
        count - usize::from(self.fieldset_frame(dom_node_id(node_id)).is_some())
    }

    fn get_child_id(&self, node_id: NodeId, index: usize) -> NodeId {
        let children = self.node_from_id(node_id).layout_children.borrow();
        let children = children.as_ref().unwrap();
        match self.fieldset_frame(dom_node_id(node_id)) {
            Some(frame) => taffy_node_id(
                children
                    .iter()
                    .copied()
                    .filter(|&id| id != frame.legend)
                    .nth(index)
                    .unwrap(),
            ),
            None => taffy_node_id(children[index]),
        }
    }
}
impl TraverseTree for LayoutPassState<'_> {}

impl LayoutPartialTree for LayoutPassState<'_> {
    type CoreContainerStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_> {
        #[cfg(feature = "writing-mode")]
        {
            // Container styles are read by the node's own algorithm (or, for the containing block
            // of an out-of-flow box, by that box's algorithm) and are expressed in the node's writing mode.
            let dom_id = dom_node_id(node_id);
            let mut style = self.nodes[dom_id].layout_style_in(self.layout_wm_of(dom_id));
            style.set_percent_basis(self.orthogonal_percent_basis_of(dom_id));
            style.legend_band = self.fieldset_frame(dom_id).map(|frame| frame.band);
            style
        }
        #[cfg(not(feature = "writing-mode"))]
        {
            let mut style = self.node_from_id(node_id).layout_style();
            style.legend_band = self
                .fieldset_frame(dom_node_id(node_id))
                .map(|frame| frame.band);
            style
        }
    }

    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout) {
        *self.node_from_id_mut(node_id).unrounded_layout_mut() = *layout;
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    #[inline(always)]
    fn compute_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::LayoutInput,
    ) -> taffy::LayoutOutput {
        compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
            tree.compute_child_layout_internal(node_id, inputs, None)
        })
    }
}

impl LayoutContainingBlock for LayoutPassState<'_> {
    type OofItemStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    fn get_oof_item_style(&self, node_id: NodeId) -> Self::OofItemStyle<'_> {
        self.child_layout_style(self.node_from_id(node_id))
    }

    fn clear_hoisted_children(&mut self, node_id: NodeId) {
        let containing_block = dom_node_id(node_id);
        let mut hoisted = self.node_from_id(node_id).hoisted_children.borrow_mut();
        for &hoisted_id in hoisted.iter() {
            // A box which has since been claimed by another containing block
            // must keep pointing at that one.
            if let Some(node) = self.nodes.get(hoisted_id) {
                if node.oof_containing_block.get() == Some(containing_block) {
                    node.oof_containing_block.set(None);
                }
            }
        }
        hoisted.clear();
    }

    fn add_hoisted_children(&mut self, node_id: NodeId, hoisted: &[NodeId]) {
        let containing_block = dom_node_id(node_id);
        let node = self.node_from_id(node_id);
        node.hoisted_children
            .borrow_mut()
            .extend(hoisted.iter().copied().map(dom_node_id));
        for &hoisted_id in hoisted {
            self.node_from_id(hoisted_id)
                .oof_containing_block
                .set(Some(containing_block));
        }
    }

    fn get_detailed_layout_info(&self, node_id: NodeId) -> &DetailedLayoutInfo<Atom> {
        self.node_from_id(node_id)
            .element_data()
            .map(|element| &element.detailed_layout_info)
            .unwrap_or(&DetailedLayoutInfo::None)
    }
}

impl taffy::CacheTree for LayoutPassState<'_> {
    #[inline]
    fn cache_get(
        &mut self,
        node_id: NodeId,
        inputs: &taffy::LayoutInput,
    ) -> Option<taffy::LayoutOutput> {
        self.node_from_id_mut(node_id)
            .try_layout_data_mut()?
            .cache
            .get(inputs)
    }

    #[inline]
    fn cache_store(
        &mut self,
        node_id: NodeId,
        inputs: &taffy::LayoutInput,
        layout_output: taffy::LayoutOutput,
    ) {
        self.node_from_id_mut(node_id)
            .cache_mut()
            .store(inputs, layout_output);
    }

    #[inline]
    fn cache_clear(&mut self, node_id: NodeId) {
        self.node_from_id_mut(node_id).clear_layout_cache();
    }
}

impl taffy::LayoutBlockContainer for LayoutPassState<'_> {
    type BlockContainerStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    type BlockItemStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    fn get_block_container_style(&self, node_id: NodeId) -> Self::BlockContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_block_child_style(&self, child_node_id: NodeId) -> Self::BlockItemStyle<'_> {
        self.child_layout_style(self.node_from_id(child_node_id))
    }

    #[inline(always)]
    fn compute_block_child_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        compute_cached_layout(self, node_id, inputs, |tree, node_id, inputs| {
            tree.compute_child_layout_internal(node_id, inputs, block_ctx)
        })
    }
}

impl taffy::LayoutFlexboxContainer for LayoutPassState<'_> {
    type FlexboxContainerStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    type FlexboxItemStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    fn get_flexbox_container_style(&self, node_id: NodeId) -> Self::FlexboxContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_flexbox_child_style(&self, child_node_id: NodeId) -> Self::FlexboxItemStyle<'_> {
        self.child_layout_style(self.node_from_id(child_node_id))
    }
}

impl taffy::LayoutGridContainer for LayoutPassState<'_> {
    type GridContainerStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = TaffyStyloStyle<ComputedStyleRef<'a>>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: NodeId) -> Self::GridItemStyle<'_> {
        self.child_layout_style(self.node_from_id(child_node_id))
    }

    fn set_detailed_grid_info(
        &mut self,
        node_id: NodeId,
        detailed_grid_info: taffy::DetailedGridInfo<Atom>,
    ) {
        let node = self.node_from_id_mut(node_id);
        if let Some(element) = node.element_data_mut() {
            element.detailed_layout_info = DetailedLayoutInfo::Grid(Box::new(detailed_grid_info));
        }
    }
}

impl RoundTree for LayoutPassState<'_> {
    fn get_unrounded_layout(&self, node_id: NodeId) -> Layout {
        *self.node_from_id(node_id).unrounded_layout()
    }

    fn set_final_layout(&mut self, node_id: NodeId, layout: &Layout) {
        *self.node_from_id_mut(node_id).final_layout_mut() = *layout;
    }

    fn is_out_of_flow(&self, node_id: NodeId) -> bool {
        self.node_from_id(node_id)
            .out_of_flow
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    fn hoisted_child_count(&self, node_id: NodeId) -> usize {
        // acrender: a box claimed by another containing block, then removed, is scrubbed from that one
        // only; a containing block that was not laid out again still lists it (closing CodeMirror)
        let mut hoisted = self.node_from_id(node_id).hoisted_children.borrow_mut();
        hoisted.retain(|id| self.nodes.contains_key(*id));
        hoisted.len()
    }

    fn get_hoisted_child_id(&self, node_id: NodeId, index: usize) -> NodeId {
        taffy_node_id(self.node_from_id(node_id).hoisted_children.borrow()[index])
    }
}

// pub struct ChildIter<'a>(std::slice::Iter<'a, usize>);
// impl<'a> Iterator for ChildIter<'a> {
//     type Item = NodeId;
//     fn next(&mut self) -> Option<Self::Item> {
//         self.0.next().copied().map(NodeId::from)
//     }
// }

pub struct RefCellChildIter<'a> {
    items: Ref<'a, [crate::NodeId]>,
    idx: usize,
    /// acrender: a child left out (a fieldset's rendered legend)
    skip: Option<crate::NodeId>,
}
impl<'a> RefCellChildIter<'a> {
    fn new(items: Ref<'a, [crate::NodeId]>) -> RefCellChildIter<'a> {
        RefCellChildIter {
            items,
            idx: 0,
            skip: None,
        }
    }
}

impl Iterator for RefCellChildIter<'_> {
    type Item = NodeId;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let id = *self.items.get(self.idx)?;
            self.idx += 1;
            if Some(id) != self.skip {
                return Some(taffy_node_id(id));
            }
        }
    }
}

/// acrender: whether `field-sizing: content` applies
pub(crate) fn field_sized(node: &Node) -> bool {
    use style::computed_values::field_sizing::T as FieldSizing;
    node.primary_styles()
        .is_some_and(|style| style.clone_field_sizing() == FieldSizing::Content)
}

/// acrender: the content width a `field-sizing: content` textarea wraps its text at: the width it is given, its
/// definite `width`, or the space there is (None: unwrapped, for its max-content size)
fn text_field_wrap(node: &Node, inputs: taffy::tree::LayoutInput) -> Option<f32> {
    use taffy::MaybeResolve as _;
    let style = node.layout_style();
    let parent = inputs.parent_size.width;
    let pb = (style.padding().resolve_or_zero(parent, resolve_calc_value)
        + style.border().resolve_or_zero(parent, resolve_calc_value))
    .horizontal_axis_sum();
    let border_box = style.box_sizing() == taffy::BoxSizing::BorderBox;
    let given = inputs.known_dimensions.width.map(|w| w - pb).or_else(|| {
        style
            .size()
            .width
            .maybe_resolve(parent, resolve_calc_value)
            .map(|w| if border_box { w - pb } else { w })
    });
    match (given, inputs.available_space.width) {
        (Some(width), _) => Some(width.max(0.0)),
        (None, AvailableSpace::Definite(limit)) => Some((limit - pb).max(0.0)),
        (None, AvailableSpace::MinContent) => Some(0.0),
        (None, AvailableSpace::MaxContent) => None,
    }
}

/// acrender: the text of a text input or textarea (its placeholder while the value is empty) laid out at `width`
/// (None: unwrapped): its widest line and its height, in CSS px
fn measure_text_field(doc: &mut BaseDocument, node: crate::NodeId, width: Option<f32>) -> Option<(f32, f32)> {
    let scale = doc.viewport.scale();
    let BaseDocument { nodes, font_ctx, layout_ctx, .. } = doc;
    let input = nodes[node].data.downcast_element_mut()?.text_input_data_mut()?;
    let editor = match (input.editor.raw_text().is_empty(), input.placeholder.as_mut()) {
        (true, Some(placeholder)) => placeholder,
        _ => &mut input.editor,
    };
    editor.set_width(width.map(|w| w * scale));
    editor.refresh_layout(&mut font_ctx.lock().unwrap(), layout_ctx);
    let layout = editor.try_layout()?;
    Some((layout.full_width() / scale, layout.height() / scale))
}
