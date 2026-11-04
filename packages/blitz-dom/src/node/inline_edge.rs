//! acrender: the start and end edges of an inline element (`<span style="padding: 1px 6px; border-radius: 5px">`).
//! Parley lays out text and inline boxes, not the padding / border / margin of the spans around them, so
//! construction puts a zero-height inline box at both ends of an element that has any of those (or a
//! rounded background), layout gives each the width of its side's margin + border + padding, and paint
//! draws the element's background and border between them, per line (blitz-paint `text.rs`).
//! Their ids are the element's node id with bit 63 set (bit 62 for the end); slotmap versions (the high
//! 32 bits of a node id) are odd counters that never get near bit 63.
use blitz_traits::node_id::NodeId;
use style::Zero as _;
use style::properties::ComputedValues;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InlineEdge {
    pub node: NodeId,
    pub end: bool,
}

impl InlineEdge {
    const FLAG: u64 = 1 << 63;
    const END: u64 = 1 << 62;

    /// The inline box id of this edge
    pub fn id(self) -> u64 {
        self.node.as_u64() | Self::FLAG | if self.end { Self::END } else { 0 }
    }

    /// The edge an inline box id stands for, if it is one
    pub fn of(id: u64) -> Option<Self> {
        (id & Self::FLAG != 0).then(|| Self {
            node: NodeId::from_u64(id & !(Self::FLAG | Self::END)),
            end: id & Self::END != 0,
        })
    }

    /// Whether an inline element needs edges: horizontal padding / margin, any border, or a background
    /// with rounded corners
    pub fn needed(style: &ComputedValues) -> bool {
        let (p, m, b) = (style.get_padding(), style.get_margin(), style.get_border());
        let margin = |v: &style::values::computed::Margin| matches!(v, style::values::generics::length::GenericMargin::LengthPercentage(l) if !l.is_zero());
        let border = [
            (&b.border_top_width, b.border_top_style),
            (&b.border_right_width, b.border_right_style),
            (&b.border_bottom_width, b.border_bottom_style),
            (&b.border_left_width, b.border_left_style),
        ]
        .iter()
        .any(|(w, s)| !s.none_or_hidden() && w.0 > app_units::Au(0));
        let radius = [
            &b.border_top_left_radius,
            &b.border_top_right_radius,
            &b.border_bottom_right_radius,
            &b.border_bottom_left_radius,
        ]
        .iter()
        .any(|r| !r.0.width.0.is_zero() || !r.0.height.0.is_zero());
        let background = style
            .get_background()
            .background_color
            .resolve_to_absolute(&style.clone_color())
            .alpha
            > 0.0;
        !p.padding_left.0.is_zero()
            || !p.padding_right.0.is_zero()
            || margin(&m.margin_left)
            || margin(&m.margin_right)
            || border
            || (radius && background)
    }
}
