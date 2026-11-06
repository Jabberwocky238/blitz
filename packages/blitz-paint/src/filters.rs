use blitz_dom::util::ToColorColor as _;
use style::color::AbsoluteColor;
pub(crate) use style::computed_values::filter::single_value::T as StyloFilter;

use acde_gpu::{ColorMatrix, DropShadow, Filter, FilterEffect, GaussianBlur};

pub(crate) fn convert_filters(filters: &[StyloFilter]) -> Option<Filter> {
    if filters.is_empty() {
        return None;
    }

    Some(filters.iter().filter_map(convert_single_filter).collect())
}

pub(crate) fn convert_single_filter(filter: &StyloFilter) -> Option<FilterEffect> {
    Some(match filter {
        StyloFilter::Blur(radius) => GaussianBlur { std_deviation: radius.px() }.into(),
        StyloFilter::Brightness(amount) => ColorMatrix::brightness(amount.0).into(),
        StyloFilter::Contrast(amount) => ColorMatrix::contrast(amount.0).into(),
        StyloFilter::Grayscale(amount) => ColorMatrix::grayscale(amount.0).into(),
        StyloFilter::HueRotate(angle) => ColorMatrix::hue_rotate(angle.radians()).into(),
        StyloFilter::Invert(amount) => ColorMatrix::invert(amount.0).into(),
        StyloFilter::Opacity(amount) => ColorMatrix::opacity(amount.0).into(),
        StyloFilter::Saturate(amount) => ColorMatrix::saturate(amount.0).into(),
        StyloFilter::Sepia(amount) => ColorMatrix::sepia(amount.0).into(),
        StyloFilter::DropShadow(shadow) => DropShadow {
            dx: shadow.horizontal.px(),
            dy: shadow.vertical.px(),
            std_deviation: shadow.blur.px(),
            // TODO: pass in correct currentColor
            color: shadow
                .color
                .resolve_to_absolute(&AbsoluteColor::BLACK)
                .as_color_color(),
        }
        .into(),
        StyloFilter::Url(_) => return None,
    })
}
