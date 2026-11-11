#![allow(clippy::module_inception)]

mod attributes;
mod element;
mod inline_edge; // acrender
mod node;
mod select; // acrender
pub(crate) mod scrollbar;
mod serialize;
#[cfg(feature = "svg")]
mod simple_svg;
mod stylo_data;
#[cfg(feature = "svg")]
mod svg;
mod text;

pub use attributes::{Attribute, Attributes};
pub(crate) use element::parse_classes;
pub use element::{
    DocumentData, ElementData, ImageData, ImageResourceData, LayoutData, ListItemLayout,
    ListItemLayoutPosition, Marker, RasterImageData, SpecialElementData, SpecialElementType,
    Status,
};
pub use inline_edge::InlineEdge;
pub use node::*;
pub use select::{SelectData, SelectItem, SelectItemKind, SelectPicker};
pub use scrollbar::{ScrollbarColor, ScrollbarRef, ScrollbarWidth};
#[cfg(feature = "svg")]
pub use simple_svg::{Rgba, SimpleOp, SimpleSvg};
pub use stylo_data::ComputedStyleRef;
#[cfg(feature = "svg")]
pub use svg::{SvgImageData, SvgIntrinsicDimensions};
#[cfg(feature = "svg")]
pub(crate) use svg::{external_uses, inline_external_uses};
pub use text::{TextBrush, TextInputData, TextLayout};
