//! acrender: box construction for `<select>` (https://html.spec.whatwg.org/#the-select-element-2).
//! Its options are not boxes (`option { display: none }`): the select is a leaf whose rows are
//! laid out here, one parley layout per option or optgroup label, in the select's own font.

use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use style_dom::ElementState;

use crate::node::{SelectData, SelectItem, SelectItemKind, SpecialElementData};
use crate::{BaseDocument, stylo_to_parley};

/// The rules for parsing non-negative integers (https://html.spec.whatwg.org/#rules-for-parsing-non-negative-integers)
pub fn parse_non_negative_integer(value: &str) -> Option<u32> {
    let rest = value.trim_start_matches(['\t', '\n', '\x0C', '\r', ' ']);
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    match digits {
        0 => None,
        _ => Some(
            rest[..digits]
                .parse::<u64>()
                .map_or(u32::MAX, |n| n.min(u32::MAX as u64) as u32),
        ),
    }
}

/// The display size and whether the select is shown as a list box
pub fn select_mode(multiple: bool, size: Option<&str>) -> (u32, bool) {
    let size = size
        .and_then(parse_non_negative_integer)
        .filter(|&n| n > 0)
        .unwrap_or(if multiple { 4 } else { 1 });
    (size, multiple || size > 1)
}

/// An option's label: its `label` attribute when not empty, else its text with whitespace stripped and collapsed
fn label(doc: &BaseDocument, id: NodeId) -> String {
    let node = &doc.nodes[id];
    if let Some(label) = node.attr(local_name!("label")).filter(|l| !l.is_empty()) {
        return label.to_owned();
    }
    node.text_content()
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

struct Row {
    node: NodeId,
    kind: SelectItemKind,
    grouped: bool,
    disabled: bool,
    selected: bool,
    text: String,
}

fn rows(doc: &BaseDocument, select: NodeId) -> Vec<Row> {
    let is = |id: NodeId, name: &str| {
        doc.nodes[id]
            .data
            .downcast_element()
            .is_some_and(|e| *e.name.local == *name)
    };
    let has = |id: NodeId, name| doc.nodes[id].attr(name).is_some();
    let option = |id: NodeId, grouped: bool, group_disabled: bool| Row {
        node: id,
        kind: SelectItemKind::Option,
        grouped,
        disabled: group_disabled || has(id, local_name!("disabled")),
        selected: doc.nodes[id]
            .element_data()
            .is_some_and(|e| e.element_state.contains(ElementState::CHECKED)),
        text: label(doc, id),
    };
    let mut rows = Vec::new();
    for &child in &doc.nodes[select].children {
        if is(child, "option") {
            rows.push(option(child, false, false));
        } else if is(child, "optgroup") {
            let disabled = has(child, local_name!("disabled"));
            let text = doc.nodes[child]
                .attr(local_name!("label"))
                .unwrap_or_default()
                .to_owned();
            rows.push(Row {
                node: child,
                kind: SelectItemKind::Group,
                grouped: false,
                disabled,
                selected: false,
                text,
            });
            for &inner in &doc.nodes[child].children {
                if is(inner, "option") {
                    rows.push(option(inner, true, disabled));
                }
            }
        }
    }
    rows
}

pub(crate) fn create_select(doc: &mut BaseDocument, select: NodeId) {
    let Some(styles) = doc.nodes[select].primary_styles().map(|s| (*s).clone()) else {
        return;
    };
    let element = doc.nodes[select]
        .element_data()
        .expect("a select is an element");
    let (size, list_box) = select_mode(
        element.has_attr(local_name!("multiple")),
        element.attr(local_name!("size")),
    );
    let rows = rows(doc, select);

    let font_size = styles.clone_font_size().used_size().px();
    let row_height = {
        use style::values::computed::font::LineHeight;
        match styles.clone_line_height() {
            LineHeight::Normal => font_size * 1.2,
            LineHeight::Number(num) => font_size * num.0,
            LineHeight::Length(value) => value.0.px(),
        }
    };
    let scale = doc.viewport.scale();
    let mut parley_style = stylo_to_parley::style(select, &styles);
    parley_style.white_space_collapse = parley::WhiteSpaceCollapse::Preserve;
    parley_style.text_wrap_mode = parley::TextWrapMode::NoWrap;

    let mut items = Vec::with_capacity(rows.len());
    {
        let mut font_ctx = doc.font_ctx.lock().unwrap();
        for row in rows {
            let mut style = parley_style.clone();
            if row.kind == SelectItemKind::Group {
                style.font_weight = parley::FontWeight::BOLD;
            }
            let mut builder = doc
                .layout_ctx
                .tree_builder(&mut font_ctx, scale, true, &style);
            builder.push_text(&row.text);
            let mut layout = builder.build().0;
            layout.break_all_lines(None);
            items.push(SelectItem {
                node: row.node,
                kind: row.kind,
                grouped: row.grouped,
                disabled: row.disabled,
                selected: row.selected,
                layout,
            });
        }
    }
    let mut data = SelectData {
        list_box,
        size,
        items,
        shown: None,
        label_width: 0.0,
        row_height,
        font_size,
    };
    let indent = data.indent();
    data.label_width = data
        .items
        .iter()
        .map(|item| item.layout.width() / scale + if item.grouped { indent } else { 0.0 })
        .fold(0.0, f32::max);
    let shown = data
        .options()
        .find(|(_, item)| item.selected)
        .map(|(i, _)| i);
    data.shown = shown;

    let element = doc.nodes[select]
        .element_data_mut()
        .expect("a select is an element");
    element.special_data = SpecialElementData::Select(Box::new(data));
    element.repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_negative_integers() {
        assert_eq!(parse_non_negative_integer("4"), Some(4));
        assert_eq!(parse_non_negative_integer("  +7px"), Some(7));
        assert_eq!(parse_non_negative_integer("-1"), None);
        assert_eq!(parse_non_negative_integer(""), None);
        assert_eq!(parse_non_negative_integer("x3"), None);
    }

    #[test]
    fn display_size() {
        assert_eq!(select_mode(false, None), (1, false));
        assert_eq!(select_mode(true, None), (4, true));
        assert_eq!(select_mode(false, Some("3")), (3, true));
        assert_eq!(select_mode(false, Some("0")), (1, false));
        assert_eq!(select_mode(true, Some("1")), (1, true));
        assert_eq!(select_mode(false, Some("nope")), (1, false));
    }
}
