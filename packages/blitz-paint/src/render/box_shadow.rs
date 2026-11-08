use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use acde_gpu::PaintScene;
use kurbo::{Rect, RoundedRect, Shape as _, Vec2};
use peniko::{Compose, Fill, Mix};

impl ElementCx<'_, '_> {
    pub(super) fn draw_outset_box_shadow(&self, scene: &mut impl PaintScene) {
        let box_shadow = &self.style.get_effects().box_shadow.0;
        let has_outset_shadow = box_shadow.iter().any(|s| !s.inset);
        if !has_outset_shadow {
            return;
        }

        let current_color = self.style.clone_color();
        let opacity = self.style.get_effects().opacity;
        let bg_color = self
            .style
            .get_background()
            .background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();
        let bg_is_opaque = bg_color.components[3] >= 1.0;
        let needs_clip = opacity < 1.0 || !bg_is_opaque;

        // acrender: the clip (around the border box, out to the farthest shadow) only when needed
        let shadow_clip = needs_clip.then(|| {
            let max_shadow_rect = box_shadow.iter().fold(Rect::ZERO, |prev, shadow| {
                let x = shadow.base.horizontal.px() as f64 * self.scale;
                let y = shadow.base.vertical.px() as f64 * self.scale;
                let blur = shadow.base.blur.px() as f64 * self.scale;
                let spread = shadow.spread.px() as f64 * self.scale;
                let offset = spread + blur * 2.5;

                let rect = self.frame.border_box.inflate(offset, offset) + Vec2::new(x, y);

                prev.union(rect)
            });
            self.frame.shadow_clip(max_shadow_rect)
        });

        crate::layers::maybe_with_layer(
            scene,
            shadow_clip.as_ref(),
            1.0,
            self.transform,
            None,
            None,
            |scene| {
                for shadow in box_shadow.iter().filter(|s| !s.inset).rev() {
                    let shadow_color = shadow
                        .base
                        .color
                        .resolve_to_absolute(&current_color)
                        .as_srgb_color();

                    let alpha = shadow_color.components[3];
                    if alpha != 0.0 {
                        // acrender: the offset is in the element's space (pre_translate), so an ancestor's
                        // scale / rotation applies to it too (upstream added it in screen space)
                        let transform = self.transform.pre_translate(Vec2 {
                            x: shadow.base.horizontal.px() as f64 * self.scale,
                            y: shadow.base.vertical.px() as f64 * self.scale,
                        });

                        let spread = shadow.spread.px() as f64 * self.scale;
                        // acrender: scaled like the offset and spread (upstream used CSS px)
                        let blur = shadow.base.blur.px() as f64 * self.scale;

                        // Without blur or spread the shadow is exactly the element's shape,
                        // so fill the border box path directly (preserving individual radii)
                        if blur == 0.0 && spread == 0.0 {
                            scene.fill(
                                Fill::NonZero,
                                transform,
                                shadow_color,
                                None,
                                &self.frame.border_box_path(),
                            );
                            continue;
                        }

                        // TODO draw shadows with matching individual radii instead of averaging
                        // acrender: per CSS Backgrounds 3 the spread grows the corner radius too (a square
                        // corner stays square); upstream kept the unspread radius, so a round dot's
                        // `0 0 0 6px` ring came out as a square
                        let radius = self.frame.border_radii.average();
                        let radius = if radius > 0.0 {
                            (radius + spread).max(0.0)
                        } else {
                            0.0
                        };
                        let rect = self.frame.border_box.inflate(spread, spread);

                        // Fill the color
                        scene.draw_box_shadow(transform, rect, shadow_color, radius, blur);
                    }
                }
            },
        )
    }

    pub(super) fn draw_inset_box_shadow(&self, scene: &mut impl PaintScene) {
        let current_color = self.style.clone_color();
        let box_shadow = &self.style.get_effects().box_shadow.0;
        let has_inset_shadow = box_shadow.iter().any(|s| s.inset);
        if !has_inset_shadow {
            return;
        }

        let padding_box = self.frame.padding_box_path();

        // acrender: the first shadow is on top (CSS Backgrounds 3), so paint them last to first like the outset ones
        for shadow in box_shadow.iter().filter(|s| s.inset).rev() {
            let shadow_color = shadow
                .base
                .color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            // acrender: skip just this one (upstream returned, dropping the shadows after it)
            if shadow_color == Color::TRANSPARENT {
                continue;
            }

            // TODO draw shadows with matching individual radii instead of averaging
            let radius = self.frame.border_radii.average();
            // acrender: in the element's space, see the outset shadow; scaled (upstream used CSS px)
            let transform = self.transform.pre_translate(Vec2 {
                x: shadow.base.horizontal.px() as f64 * self.scale,
                y: shadow.base.vertical.px() as f64 * self.scale,
            });

            // The unshadowed area is the padding box shrunk by the spread radius
            let spread = shadow.spread.px() as f64 * self.scale;
            let hole = self.frame.padding_box.inflate(-spread, -spread);
            let blur = shadow.base.blur.px() as f64 * self.scale;

            // acrender: a sharp shadow is one fill of the padding box minus the hole (reversed, so
            // coincident edges cancel exactly); filling the box and then cutting the hole out left a
            // faint line wherever both edges fall inside a pixel (xterm.js's bar cursor)
            if blur == 0.0 {
                let offset = Vec2 {
                    x: shadow.base.horizontal.px() as f64 * self.scale,
                    y: shadow.base.vertical.px() as f64 * self.scale,
                };
                let hole = (hole + offset).intersect(self.frame.padding_box);
                let mut shape = padding_box.clone();
                if hole.width() > 0.0 && hole.height() > 0.0 {
                    let hole = RoundedRect::from_rect(hole, radius).to_path(0.1);
                    shape.extend(hole.reverse_subpaths());
                }
                let rounded = radius > 0.0;
                if rounded {
                    scene.push_layer(
                        Fill::NonZero,
                        Mix::Normal,
                        1.0,
                        self.transform,
                        &padding_box,
                        None,
                        None,
                    );
                }
                scene.fill(Fill::NonZero, self.transform, shadow_color, None, &shape);
                if rounded {
                    scene.pop_layer();
                }
                continue;
            }

            scene.push_layer(
                Fill::NonZero,
                Mix::Normal,
                1.0,
                self.transform,
                &padding_box,
                None,
                None,
            );
            scene.fill(
                Fill::NonZero,
                self.transform,
                shadow_color,
                None,
                &padding_box,
            );

            scene.push_layer(
                Fill::NonZero,
                Compose::DestOut,
                1.0,
                self.transform,
                &padding_box,
                None,
                None,
            );
            scene.draw_box_shadow(transform, hole, Color::WHITE, radius, blur);

            scene.pop_layer();
            scene.pop_layer();
        }
    }
}
