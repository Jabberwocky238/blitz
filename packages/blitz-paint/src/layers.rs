//! acrender: pushing and popping layers. Upstream's `LayerManager` stopped pushing layers past 1024
//! per painter (silently dropping those clips, and counted differently by a fragment's recording and
//! a whole-page one) and kept accounting nothing read.
use acgpu::{Filter, PaintScene};
use kurbo::{Affine, Shape};
use peniko::{Fill, Mix};
use std::sync::Arc;

/// Pushes a clip layer, or a compositing layer when there is opacity or a filter.
pub(crate) fn push_layer(
    scene: &mut impl PaintScene,
    shape: &impl Shape,
    opacity: f32,
    transform: Affine,
    filter: Option<Arc<Filter>>,
    backdrop_filter: Option<Arc<Filter>>,
) {
    if opacity == 1.0 && filter.is_none() && backdrop_filter.is_none() {
        scene.push_clip_layer(Fill::NonZero, transform, shape);
    } else {
        scene.push_layer(
            Fill::NonZero,
            Mix::Normal,
            opacity,
            transform,
            shape,
            filter,
            backdrop_filter,
        );
    }
}

/// Paints `paint` inside a layer of `shape` (see `push_layer`), or without one when there is no
/// shape. Callers build the shape only when the layer is needed.
pub(crate) fn maybe_with_layer<S: PaintScene>(
    scene: &mut S,
    shape: Option<&impl Shape>,
    opacity: f32,
    transform: Affine,
    filter: Option<Arc<Filter>>,
    backdrop_filter: Option<Arc<Filter>>,
    paint: impl FnOnce(&mut S),
) {
    let Some(shape) = shape else {
        paint(scene);
        return;
    };
    push_layer(scene, shape, opacity, transform, filter, backdrop_filter);
    paint(scene);
    scene.pop_layer();
}
