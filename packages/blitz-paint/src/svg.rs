//! acrender: a usvg tree painted into acde-gpu's [`PaintScene`]. This was anyrender_svg 0.15 (Apache-2.0 OR MIT, the Vello
//! authors); it moved here with the scene type when anyrender went (acde-gpu owns the scene now).
//!
//! Missing, as before: masks, filter effects, patterns, group backgrounds; clip paths with more than one path. An
//! unsupported node is drawn as a half-transparent red box.

use acde_gpu::{Paint, PaintScene};
use kurbo::{Affine, BezPath, Cap, Join, Point, Rect, Stroke};
use peniko::color::{DynamicColor, palette};
use peniko::{BlendMode, Blob, Color, ColorStop, Compose, Extend, Fill, Gradient, ImageAlphaType, ImageBrush, ImageData, ImageFormat, ImageQuality, Mix};

/// Paints one tree: `global` is applied over every node's own transform.
pub(crate) struct SvgPainter<'s, S: PaintScene> {
    scene: &'s mut S,
    global: Affine,
}

/// usvg's values as kurbo / peniko ones (both sides are foreign types, so not `From`).
struct Usvg;

impl Usvg {
    fn affine(t: &usvg::Transform) -> Affine {
        Affine::new([t.sx, t.ky, t.kx, t.sy, t.tx, t.ty].map(f64::from))
    }

    fn fill(rule: usvg::FillRule) -> Fill {
        match rule {
            usvg::FillRule::NonZero => Fill::NonZero,
            usvg::FillRule::EvenOdd => Fill::EvenOdd,
        }
    }

    fn mix(mode: usvg::BlendMode) -> Mix {
        use usvg::BlendMode as B;
        match mode {
            B::Normal => Mix::Normal,
            B::Multiply => Mix::Multiply,
            B::Screen => Mix::Screen,
            B::Overlay => Mix::Overlay,
            B::Darken => Mix::Darken,
            B::Lighten => Mix::Lighten,
            B::ColorDodge => Mix::ColorDodge,
            B::ColorBurn => Mix::ColorBurn,
            B::HardLight => Mix::HardLight,
            B::SoftLight => Mix::SoftLight,
            B::Difference => Mix::Difference,
            B::Exclusion => Mix::Exclusion,
            B::Hue => Mix::Hue,
            B::Saturation => Mix::Saturation,
            B::Color => Mix::Color,
            B::Luminosity => Mix::Luminosity,
        }
    }

    fn extend(spread: usvg::SpreadMethod) -> Extend {
        match spread {
            usvg::SpreadMethod::Pad => Extend::Pad,
            usvg::SpreadMethod::Reflect => Extend::Reflect,
            usvg::SpreadMethod::Repeat => Extend::Repeat,
        }
    }

    fn stroke(stroke: &usvg::Stroke) -> Stroke {
        let cap = match stroke.linecap() {
            usvg::LineCap::Butt => Cap::Butt,
            usvg::LineCap::Round => Cap::Round,
            usvg::LineCap::Square => Cap::Square,
        };
        let join = match stroke.linejoin() {
            usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => Join::Miter,
            usvg::LineJoin::Round => Join::Round,
            usvg::LineJoin::Bevel => Join::Bevel,
        };
        let out = Stroke::new(f64::from(stroke.width().get())).with_caps(cap).with_join(join).with_miter_limit(f64::from(stroke.miterlimit().get()));
        match stroke.dasharray() {
            Some(dashes) => out.with_dashes(f64::from(stroke.dashoffset()), dashes.iter().map(|d| f64::from(*d))),
            None => out,
        }
    }

    /// SVG paths reopen at the last move-to after a close; `BezPath` needs that move-to spelled out.
    fn path(path: &usvg::Path) -> BezPath {
        use usvg::tiny_skia_path::PathSegment as Seg;
        let point = |p: usvg::tiny_skia_path::Point| Point::new(f64::from(p.x), f64::from(p.y));
        let mut out = BezPath::new();
        let (mut closed, mut start) = (false, Point::ZERO);
        for segment in path.data().segments() {
            if !matches!(segment, Seg::Close) && std::mem::take(&mut closed) {
                out.move_to(start);
            }
            match segment {
                Seg::MoveTo(p) => {
                    start = point(p);
                    out.move_to(start);
                }
                Seg::LineTo(p) => out.line_to(point(p)),
                Seg::QuadTo(p1, p2) => out.quad_to(point(p1), point(p2)),
                Seg::CubicTo(p1, p2, p3) => out.curve_to(point(p1), point(p2), point(p3)),
                Seg::Close => {
                    closed = true;
                    out.close_path();
                }
            }
        }
        out
    }

    fn stops(stops: &[usvg::Stop], opacity: usvg::Opacity) -> Vec<ColorStop> {
        let color = |s: &usvg::Stop| Color::from_rgba8(s.color().red, s.color().green, s.color().blue, (s.opacity() * opacity).to_u8());
        stops.iter().map(|s| ColorStop { offset: s.offset().get(), color: DynamicColor::from_alpha_color(color(s)) }).collect()
    }

    /// The paint and its transform; `None` for patterns.
    fn paint(paint: &usvg::Paint, opacity: usvg::Opacity) -> Option<(Paint, Affine)> {
        Some(match paint {
            usvg::Paint::Color(c) => (Paint::Solid(Color::from_rgba8(c.red, c.green, c.blue, opacity.to_u8())), Affine::IDENTITY),
            usvg::Paint::LinearGradient(g) => {
                let (start, end) = (Point::new(f64::from(g.x1()), f64::from(g.y1())), Point::new(f64::from(g.x2()), f64::from(g.y2())));
                let gradient = Gradient::new_linear(start, end).with_stops(Self::stops(g.stops(), opacity).as_slice()).with_extend(Self::extend(g.spread_method()));
                (Paint::Gradient(gradient), Self::affine(&g.transform()))
            }
            usvg::Paint::RadialGradient(g) => {
                let (focus, center) = (Point::new(f64::from(g.fx()), f64::from(g.fy())), Point::new(f64::from(g.cx()), f64::from(g.cy())));
                let gradient = Gradient::new_two_point_radial(focus, g.fr().get(), center, g.r().get())
                    .with_stops(Self::stops(g.stops(), opacity).as_slice())
                    .with_extend(Self::extend(g.spread_method()));
                (Paint::Gradient(gradient), Self::affine(&g.transform()))
            }
            usvg::Paint::Pattern(_) => return None,
        })
    }

    /// A raster image embedded in the SVG, decoded to RGBA8.
    fn image(kind: &usvg::ImageKind, rendering: usvg::ImageRendering) -> Option<ImageBrush> {
        let (data, format) = match kind {
            usvg::ImageKind::JPEG(d) => (d, image::ImageFormat::Jpeg),
            usvg::ImageKind::PNG(d) => (d, image::ImageFormat::Png),
            usvg::ImageKind::GIF(d) => (d, image::ImageFormat::Gif),
            usvg::ImageKind::WEBP(d) => (d, image::ImageFormat::WebP),
            usvg::ImageKind::SVG(_) => return None,
        };
        let decoded = image::load_from_memory_with_format(data, format).ok()?.into_rgba8();
        let quality = match rendering {
            usvg::ImageRendering::HighQuality => ImageQuality::High,
            usvg::ImageRendering::OptimizeQuality | usvg::ImageRendering::Smooth => ImageQuality::Medium,
            usvg::ImageRendering::OptimizeSpeed | usvg::ImageRendering::CrispEdges | usvg::ImageRendering::Pixelated => ImageQuality::Low,
        };
        let (width, height) = decoded.dimensions();
        let data = Blob::new(std::sync::Arc::new(decoded.into_vec()));
        Some(ImageBrush::new(ImageData { data, format: ImageFormat::Rgba8, alpha_type: ImageAlphaType::Alpha, width, height }).with_quality(quality))
    }
}

impl<'s, S: PaintScene> SvgPainter<'s, S> {
    /// Paints `tree` into `scene` under `transform`.
    pub(crate) fn draw(scene: &'s mut S, tree: &usvg::Tree, transform: Affine) {
        Self { scene, global: transform }.group(tree.root(), Affine::IDENTITY);
    }

    fn group(&mut self, group: &usvg::Group, transform: Affine) {
        for node in group.children() {
            let transform = transform * Usvg::affine(&node.abs_transform());
            match node {
                usvg::Node::Group(g) => self.layered(g, self.global * transform),
                usvg::Node::Path(path) if path.is_visible() => {
                    let shape = Usvg::path(path);
                    let at = self.global * transform;
                    match path.paint_order() {
                        usvg::PaintOrder::FillAndStroke => {
                            self.fill(path, at, &shape, node);
                            self.stroke(path, at, &shape, node);
                        }
                        usvg::PaintOrder::StrokeAndFill => {
                            self.stroke(path, at, &shape, node);
                            self.fill(path, at, &shape, node);
                        }
                    }
                }
                usvg::Node::Image(img) if img.is_visible() => match img.kind() {
                    usvg::ImageKind::SVG(svg) => self.group(svg.root(), transform),
                    kind => match Usvg::image(kind, img.rendering_mode()) {
                        Some(image) => self.scene.draw_image(image.as_ref(), self.global * Usvg::affine(&img.abs_transform())),
                        None => self.unsupported(node),
                    },
                },
                // the flattened text's children already carry the text node's transform
                usvg::Node::Text(text) => self.group(text.flattened(), Affine::IDENTITY),
                usvg::Node::Path(_) | usvg::Node::Image(_) => {}
            }
        }
    }

    /// A group, in a layer when it clips (a single-path clip-path) or blends / is translucent; `at` is the group's
    /// transform on the canvas.
    /// acrender: a mask that is one solid, unstroked path is that path as a clip, at the alpha its fill gives the mask
    /// (alpha masks: the fill's opacity; luminance masks: the color's luminance times that): the usual icon export
    /// (`<mask style="mask-type:alpha"><path fill="#000" .../></mask>`). Any other mask clips to its region only;
    /// vello takes no mask layers.
    fn mask_clips(&mut self, g: &usvg::Group) -> usize {
        let Some(mask) = g.mask() else { return 0 };
        let r = mask.rect();
        let region = Rect::new(f64::from(r.left()), f64::from(r.top()), f64::from(r.right()), f64::from(r.bottom()));
        self.scene.push_clip_layer(Fill::NonZero, self.global * Usvg::affine(&g.abs_transform()), &region);
        let [usvg::Node::Path(path)] = mask.root().children() else { return 1 };
        let alpha = path.fill().filter(|_| path.stroke().is_none() && mask.mask().is_none()).and_then(|f| {
            let usvg::Paint::Color(c) = f.paint() else { return None };
            let coverage = match mask.kind() {
                usvg::MaskType::Alpha => 1.0,
                usvg::MaskType::Luminance => {
                    (0.2125 * f32::from(c.red) + 0.7154 * f32::from(c.green) + 0.0721 * f32::from(c.blue)) / 255.0
                }
            };
            Some(coverage * f.opacity().get())
        });
        let Some(alpha) = alpha else { return 1 };
        let fill = path.fill().map_or(Fill::NonZero, |f| Usvg::fill(f.rule()));
        let at = self.global * Usvg::affine(&path.abs_transform());
        self.scene.push_layer(fill, BlendMode::default(), alpha, at, &Usvg::path(path), None, None);
        2
    }

    /// acrender: a group's filters as the compositor's effects, in its user space (the layer's transform scales them):
    /// Gaussian blur, drop shadow and color matrices chained from SourceGraphic, with Figma's no-op prefix (a
    /// transparent feFlood and SourceGraphic feBlend-ed over it) let through. None for anything else (drawn unfiltered).
    fn filter(g: &usvg::Group) -> Option<(std::sync::Arc<acde_gpu::Filter>, Rect)> {
        use acde_gpu::{ColorMatrix, DropShadow, FilterEffect, GaussianBlur};
        use usvg::filter::{ColorMatrixKind, Input, Kind};
        let mut effects: Vec<FilterEffect> = Vec::new();
        let mut region: Option<Rect> = None;
        for filter in g.filters() {
            let r = filter.rect();
            let rect = Rect::new(f64::from(r.left()), f64::from(r.top()), f64::from(r.right()), f64::from(r.bottom()));
            region = Some(region.map_or(rect, |all| all.union(rect)));
            let mut chain: Option<&str> = None;
            let mut transparent: Vec<&str> = Vec::new();
            let on_chain = |input: &Input, chain: Option<&str>| match input {
                Input::SourceGraphic => chain.is_none(),
                Input::Reference(name) => chain == Some(name.as_str()),
                Input::SourceAlpha => false,
            };
            for primitive in filter.primitives() {
                let effect: Option<FilterEffect> = match primitive.kind() {
                    Kind::Flood(f) if f.opacity().get() <= 0.0 => {
                        transparent.push(primitive.result());
                        continue;
                    }
                    Kind::Blend(b)
                        if b.mode() == usvg::BlendMode::Normal
                            && on_chain(b.input1(), chain)
                            && matches!(b.input2(), Input::Reference(name) if transparent.contains(&name.as_str())) =>
                    {
                        None
                    }
                    Kind::GaussianBlur(b) if on_chain(b.input(), chain) && b.std_dev_x() == b.std_dev_y() => {
                        Some(GaussianBlur { std_deviation: b.std_dev_x().get() }.into())
                    }
                    Kind::DropShadow(d) if on_chain(d.input(), chain) && d.std_dev_x() == d.std_dev_y() => {
                        let c = d.color();
                        let color = Color::from_rgba8(c.red, c.green, c.blue, (d.opacity().get() * 255.0).round() as u8);
                        Some(DropShadow { dx: d.dx(), dy: d.dy(), std_deviation: d.std_dev_x().get(), color }.into())
                    }
                    Kind::ColorMatrix(m) if on_chain(m.input(), chain) => Some(match m.kind() {
                        ColorMatrixKind::Matrix(values) => ColorMatrix(values.as_slice().try_into().ok()?).into(),
                        ColorMatrixKind::Saturate(amount) => ColorMatrix::saturate(amount.get()).into(),
                        ColorMatrixKind::HueRotate(degrees) => ColorMatrix::hue_rotate(degrees.to_radians()).into(),
                        ColorMatrixKind::LuminanceToAlpha => return None,
                    }),
                    _ => return None,
                };
                effects.extend(effect);
                chain = Some(primitive.result());
            }
        }
        let region = region?;
        (!effects.is_empty()).then(|| (std::sync::Arc::new(effects.into_iter().collect()), region))
    }

    fn layered(&mut self, g: &usvg::Group, at: Affine) {
        let masks = self.mask_clips(g);
        let filter = Self::filter(g);
        if let Some((filter, region)) = &filter {
            let own = self.global * Usvg::affine(&g.abs_transform());
            self.scene.push_layer(Fill::NonZero, BlendMode::default(), 1.0, own, region, Some(filter.clone()), None);
        }
        self.layered_plain(g, at);
        if filter.is_some() {
            self.scene.pop_layer();
        }
        for _ in 0..masks {
            self.scene.pop_layer();
        }
    }

    fn layered_plain(&mut self, g: &usvg::Group, at: Affine) {
        let (alpha, mix) = (g.opacity().get(), Usvg::mix(g.blend_mode()));
        let blend = BlendMode { mix, compose: Compose::SrcOver };
        let layer = match g.clip_path().and_then(|clip| clip.root().children().first()) {
            Some(usvg::Node::Path(clip)) => {
                let fill = clip.fill().map_or(Fill::NonZero, |f| Usvg::fill(f.rule()));
                self.scene.push_layer(fill, blend, alpha, at, &Usvg::path(clip), None, None);
                true
            }
            _ if mix != Mix::Normal || alpha < 1.0 => {
                // the layer's bounding box on the canvas, outset by 2 px like resvg so edge anti-aliasing is not cut
                let b = g.layer_bounding_box();
                let rect = Rect::from_origin_size((f64::from(b.x()), f64::from(b.y())), (f64::from(b.width()), f64::from(b.height())));
                let rect = at.transform_rect_bbox(rect).inflate(2.0, 2.0);
                self.scene.push_layer(Fill::NonZero, blend, alpha, Affine::IDENTITY, &rect, None, None);
                true
            }
            _ => false,
        };
        self.group(g, Affine::IDENTITY);
        if layer {
            self.scene.pop_layer();
        }
    }

    fn fill(&mut self, path: &usvg::Path, at: Affine, shape: &BezPath, node: &usvg::Node) {
        let Some(fill) = path.fill() else { return };
        match Usvg::paint(fill.paint(), fill.opacity()) {
            Some((paint, paint_at)) => self.scene.fill(Usvg::fill(fill.rule()), at, &paint, Some(paint_at), shape),
            None => self.unsupported(node),
        }
    }

    fn stroke(&mut self, path: &usvg::Path, at: Affine, shape: &BezPath, node: &usvg::Node) {
        let Some(stroke) = path.stroke() else { return };
        match Usvg::paint(stroke.paint(), stroke.opacity()) {
            Some((paint, paint_at)) => self.scene.stroke(&Usvg::stroke(stroke), at, &paint, Some(paint_at), shape),
            None => self.unsupported(node),
        }
    }

    fn unsupported(&mut self, node: &usvg::Node) {
        let b = node.bounding_box();
        let rect = Rect::new(f64::from(b.left()), f64::from(b.top()), f64::from(b.right()), f64::from(b.bottom()));
        self.scene.fill(Fill::NonZero, Affine::IDENTITY, palette::css::RED.multiply_alpha(0.5), None, &rect);
    }
}
