use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use acde_gpu::PaintScene;
use blitz_dom::local_name;
use kurbo::{Affine, BezPath, Cap, Circle, Join, Point, RoundedRect, Stroke, Vec2};
use peniko::Fill;
use style::dom::TElement as _;

impl ElementCx<'_, '_> {
    pub(super) fn draw_input(&self, scene: &mut impl PaintScene) {
        if self.node.local_name() != "input" {
            return;
        }
        let Some(checked) = self.element.checkbox_input_checked() else {
            return;
        };

        let type_attr = self.node.attr(local_name!("type"));
        let disabled = self.node.attr(local_name!("disabled")).is_some();

        // TODO this should be coming from css accent-color, but I couldn't find how to retrieve it
        let accent_color = if disabled {
            Color::from_rgba8(209, 209, 209, 255)
        } else {
            self.style.clone_color().as_srgb_color()
        };

        let width = self.frame.border_box.width();
        let height = self.frame.border_box.height();
        let min_dimension = width.min(height);
        let scale = (min_dimension - 4.0).max(0.0) / 16.0;

        let frame = self.frame.border_box.to_rounded_rect(scale * 2.0);

        match type_attr {
            Some("checkbox") => {
                draw_checkbox(scene, checked, frame, self.transform, accent_color, scale);
            }
            Some("radio") => {
                let center = frame.center();
                draw_radio_button(scene, checked, center, self.transform, accent_color, scale);
            }
            _ => {}
        }
    }
}

fn draw_checkbox(
    scene: &mut impl PaintScene,
    checked: bool,
    frame: RoundedRect,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &frame);
        //Tick code derived from masonry
        let mut path = BezPath::new();
        path.move_to((2.0, 9.0));
        path.line_to((6.0, 13.0));
        path.line_to((14.0, 2.0));

        path.apply_affine(Affine::translate(Vec2 { x: 2.0, y: 1.0 }).then_scale(scale));

        let style = Stroke {
            width: 2.0 * scale,
            join: Join::Round,
            miter_limit: 10.0,
            start_cap: Cap::Round,
            end_cap: Cap::Round,
            dash_pattern: Default::default(),
            dash_offset: 0.0,
        };

        scene.stroke(&style, transform, Color::WHITE, None, &path);
    } else {
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &frame);
        scene.stroke(&Stroke::default(), transform, accent_color, None, &frame);
    }
}

fn draw_radio_button(
    scene: &mut impl PaintScene,
    checked: bool,
    center: Point,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    let outer_ring = Circle::new(center, 8.0 * scale);
    let gap = Circle::new(center, 6.0 * scale);
    let inner_circle = Circle::new(center, 4.0 * scale);
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
        scene.fill(Fill::NonZero, transform, accent_color, None, &inner_circle);
    } else {
        const GRAY: Color = color::palette::css::GRAY;
        scene.fill(Fill::NonZero, transform, GRAY, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
    }
}

/// acrender: list box rows and the popup's highlighted row (Chromium's selection colours)
const SELECTED_FOCUSED: Color = Color::from_rgb8(25, 103, 210);
const SELECTED_UNFOCUSED: Color = Color::from_rgb8(206, 206, 206);
const DISABLED_TEXT: Color = Color::from_rgb8(109, 109, 109);
const PICKER_EDGE: Color = Color::from_rgba8(0, 0, 0, 46);
const PICKER_SHADOW: Color = Color::from_rgba8(0, 0, 0, 64);

impl ElementCx<'_, '_> {
    /// acrender: a `<select>`: a drop-down box's first selected option and arrow, or a list box's
    /// rows (see blitz-dom `layout::select`)
    pub(super) fn draw_select(&self, scene: &mut impl PaintScene) {
        let Some(select) = self.element.select_data() else {
            return;
        };
        let color = self.style.clone_color().as_srgb_color();
        let content = self.frame.content_box;
        let doc = self.context.dom;
        let mut text = self.context.draw_text_context.borrow_mut();
        if select.list_box {
            let row = select.row_height as f64 * self.scale;
            if row <= 0.0 {
                return;
            }
            let scroll = self.node.scroll_offset().y * self.scale;
            let first = (scroll / row).floor().max(0.0) as usize;
            let last = ((scroll + self.frame.padding_box.height()) / row).ceil() as usize;
            let focused = self.node.is_focussed();
            let indent = select.indent() as f64 * self.scale;
            for (i, item) in select.items.iter().enumerate().take(last + 1).skip(first) {
                let y = content.y0 + i as f64 * row;
                let mut fg = if item.disabled { DISABLED_TEXT } else { color };
                if item.selected {
                    let band = kurbo::Rect::new(
                        self.frame.padding_box.x0,
                        y,
                        self.frame.padding_box.x1,
                        y + row,
                    );
                    scene.fill(
                        Fill::NonZero,
                        self.transform,
                        if focused {
                            SELECTED_FOCUSED
                        } else {
                            SELECTED_UNFOCUSED
                        },
                        None,
                        &band,
                    );
                    if focused {
                        fg = Color::WHITE;
                    }
                }
                let x = content.x0 + if item.grouped { indent } else { 0.0 };
                let y = y + (row - item.layout.height() as f64) / 2.0;
                text.color = Some(fg);
                crate::text::stroke_text(
                    scene,
                    item.layout.lines(),
                    doc,
                    self.transform * Affine::translate((x, y)),
                    self.scale,
                    self.node.id,
                    None,
                    &mut text,
                );
            }
            text.color = None;
            return;
        }
        let arrow = select.arrow_width() as f64 * self.scale;
        if let Some(item) = select.shown.and_then(|i| select.items.get(i)) {
            let label = kurbo::Rect::new(
                content.x0,
                content.y0,
                (content.x1 - arrow).max(content.x0),
                content.y1,
            );
            scene.push_clip_layer(Fill::NonZero, self.transform, &label);
            let y = content.y0 + (content.height() - item.layout.height() as f64) / 2.0;
            text.color = Some(color);
            crate::text::stroke_text(
                scene,
                item.layout.lines(),
                doc,
                self.transform * Affine::translate((content.x0, y)),
                self.scale,
                self.node.id,
                None,
                &mut text,
            );
            text.color = None;
            scene.pop_layer();
        }
        let size = select.font_size as f64 * 0.28 * self.scale;
        let center = Point::new(
            content.x1 - arrow / 2.0,
            content.y0 + content.height() / 2.0,
        );
        let mut chevron = BezPath::new();
        chevron.move_to((center.x - size, center.y - size / 2.0));
        chevron.line_to((center.x, center.y + size / 2.0));
        chevron.line_to((center.x + size, center.y - size / 2.0));
        let stroke = Stroke {
            width: 1.5 * self.scale,
            join: Join::Round,
            start_cap: Cap::Round,
            end_cap: Cap::Round,
            ..Stroke::default()
        };
        scene.stroke(&stroke, self.transform, color, None, &chevron);
    }
}

/// acrender: an open drop-down box's list of options, drawn on top of the page (see blitz-dom
/// `SelectPicker`)
pub(crate) fn draw_picker(
    scene: &mut impl PaintScene,
    doc: &blitz_dom::BaseDocument,
    picker: &blitz_dom::node::SelectPicker,
    scale: f64,
) {
    let Some(node) = doc.get_node(picker.select) else {
        return;
    };
    let (Some(select), Some(style)) = (
        node.element_data().and_then(|e| e.select_data()),
        node.primary_styles(),
    ) else {
        return;
    };
    let color = style.clone_color().as_srgb_color();
    let rect = kurbo::Rect::new(
        picker.rect.x0 * scale,
        picker.rect.y0 * scale,
        picker.rect.x1 * scale,
        picker.rect.y1 * scale,
    );
    let radius = 6.0 * scale;
    let shape = rect.to_rounded_rect(radius);
    scene.draw_box_shadow(
        Affine::translate((0.0, 3.0 * scale)),
        rect,
        PICKER_SHADOW,
        radius,
        8.0 * scale,
    );
    scene.fill(Fill::NonZero, Affine::IDENTITY, Color::WHITE, None, &shape);
    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &shape);
    let row = picker.row_height * scale;
    let top = rect.y0 + picker.padding * scale - picker.scroll * scale;
    let indent = select.indent() as f64 * scale;
    let pad = 10.0 * scale;
    let mut text = crate::text::DrawTextContext::default();
    for (i, item) in select.items.iter().enumerate() {
        let y = top + i as f64 * row;
        if y + row < rect.y0 || y > rect.y1 {
            continue;
        }
        let active = picker.active == Some(i) && !item.disabled;
        if active {
            let band = kurbo::Rect::new(rect.x0 + 4.0 * scale, y, rect.x1 - 4.0 * scale, y + row)
                .to_rounded_rect(4.0 * scale);
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                SELECTED_FOCUSED,
                None,
                &band,
            );
        }
        text.color = Some(if active {
            Color::WHITE
        } else if item.disabled {
            DISABLED_TEXT
        } else {
            color
        });
        let x = rect.x0 + pad + if item.grouped { indent } else { 0.0 };
        let y = y + (row - item.layout.height() as f64) / 2.0;
        crate::text::stroke_text(
            scene,
            item.layout.lines(),
            doc,
            Affine::translate((x, y)),
            scale,
            node.id,
            None,
            &mut text,
        );
    }
    scene.pop_layer();
    scene.stroke(
        &Stroke::new(scale),
        Affine::IDENTITY,
        PICKER_EDGE,
        None,
        &shape,
    );
}
