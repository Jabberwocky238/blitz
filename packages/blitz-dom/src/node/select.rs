//! acrender: what a `<select>` shows, built from its list of options when its box is constructed
//! (`layout::select`). A drop-down box shows its first selected option; a list box (`multiple`, or a
//! display size above 1) shows rows for every option and optgroup label. The embedder keeps each
//! option's selectedness in its `:checked` state (`ElementState::CHECKED`).

use blitz_traits::node_id::NodeId;
use kurbo::Rect;

use super::TextBrush;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectItemKind {
    /// An `<option>` in the select's list of options
    Option,
    /// An `<optgroup>`'s label
    Group,
}

#[derive(Clone)]
pub struct SelectItem {
    pub node: NodeId,
    pub kind: SelectItemKind,
    /// An option inside an `<optgroup>` (drawn indented)
    pub grouped: bool,
    /// The option is disabled, or its optgroup is
    pub disabled: bool,
    pub selected: bool,
    /// The label, laid out on one line in the select's font (device pixels)
    pub layout: parley::Layout<TextBrush>,
}

#[derive(Clone, Default)]
pub struct SelectData {
    /// Rows instead of a drop-down box
    pub list_box: bool,
    /// The display size: the rows a list box shows
    pub size: u32,
    pub items: Vec<SelectItem>,
    /// The item a drop-down box shows (the first selected option)
    pub shown: Option<usize>,
    /// The widest label, indent included (CSS px)
    pub label_width: f32,
    /// One row of a list box (CSS px)
    pub row_height: f32,
    /// The select's font size (CSS px)
    pub font_size: f32,
}

impl SelectData {
    /// The indent of an option inside an `<optgroup>` (CSS px)
    pub fn indent(&self) -> f32 {
        self.font_size * 1.25
    }

    /// The width a drop-down box keeps for its arrow (CSS px)
    pub fn arrow_width(&self) -> f32 {
        self.font_size * 1.25
    }

    /// The content box a drop-down box or a list box asks for (CSS px)
    pub fn content_size(&self) -> (f32, f32) {
        match self.list_box {
            true => (self.label_width, self.row_height * self.size as f32),
            false => (self.label_width + self.arrow_width(), self.row_height),
        }
    }

    /// The options, in the order of the select's list of options: (item index, item)
    pub fn options(&self) -> impl Iterator<Item = (usize, &SelectItem)> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.kind == SelectItemKind::Option)
    }

    /// The item index of the option at `index` in the list of options
    pub fn item_of_option(&self, index: usize) -> Option<usize> {
        self.options().nth(index).map(|(i, _)| i)
    }

    /// The index in the list of options of the option shown as item `item`
    pub fn option_of_item(&self, item: usize) -> Option<usize> {
        self.options().position(|(i, _)| i == item)
    }
}

/// An open drop-down box's list of options: drawn above the page (it is not in the document), in
/// viewport coordinates (CSS px). `rows` is one row per item of the select's [`SelectData`].
#[derive(Clone, Debug, PartialEq)]
pub struct SelectPicker {
    pub select: NodeId,
    pub rect: Rect,
    /// Space above the first row and below the last
    pub padding: f64,
    pub row_height: f64,
    /// How far the rows are scrolled
    pub scroll: f64,
    /// The highlighted item (an item index)
    pub active: Option<usize>,
}

impl SelectPicker {
    /// The height all rows take
    pub fn rows_height(&self, rows: usize) -> f64 {
        rows as f64 * self.row_height
    }

    /// The farthest the rows scroll
    pub fn max_scroll(&self, rows: usize) -> f64 {
        (self.rows_height(rows) + 2.0 * self.padding - self.rect.height()).max(0.0)
    }

    /// The item under a viewport point: `Err(())` outside the picker, `Ok(None)` inside it but on
    /// no row
    #[allow(clippy::result_unit_err)]
    pub fn item_at(&self, rows: usize, x: f64, y: f64) -> Result<Option<usize>, ()> {
        if !self.rect.contains(kurbo::Point::new(x, y)) {
            return Err(());
        }
        let offset = y - self.rect.y0 - self.padding + self.scroll;
        if offset < 0.0 {
            return Ok(None);
        }
        let row = (offset / self.row_height).floor() as usize;
        Ok((row < rows).then_some(row))
    }

    /// Scrolls just enough to show `item` whole
    pub fn reveal(&mut self, rows: usize, item: usize) {
        let top = item as f64 * self.row_height;
        let bottom = top + self.row_height;
        let view = self.rect.height() - 2.0 * self.padding;
        if top < self.scroll {
            self.scroll = top;
        } else if bottom > self.scroll + view {
            self.scroll = bottom - view;
        }
        self.scroll = self.scroll.clamp(0.0, self.max_scroll(rows));
    }
}
