//! Record view: one row of the grid as a list of its columns' names, types
//! and values, which reads better than a wide row. Previous and Next step
//! through the grid's rows (in its view), moving the grid's selection along.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    App, ClickEvent, ClipboardItem, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use super::grid::focused_cell;
use super::{ResultTable, Results};
use crate::format::format_count;

/// The most of one value the list shows.
const MAX_VALUE_CHARS: usize = 2_000;

impl Results {
    /// Show the selected row as a record (⌘I).
    pub fn show_record(&mut self, window: &mut Window, cx: &mut gpui_kit::Context<Self>) {
        let Some(table) = self.active_table() else {
            return;
        };
        let row = focused_cell(table.read(cx)).map_or(0, |(row, _)| row);
        if table.read(cx).delegate().view_len() == 0 {
            return;
        }
        let at = Rc::new(Cell::new(row));
        window.open_dialog(cx, move |dialog, _, cx| {
            let (fields, rows) = fields(&table, at.get(), cx);
            let (back, next) = ((table.clone(), at.clone()), (table.clone(), at.clone()));
            dialog
                .title("Record View")
                .w(px(720.))
                .child(
                    v_flex()
                        .id("record-fields")
                        .max_h(px(520.))
                        .overflow_y_scroll()
                        .gap_0p5()
                        .children(fields),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "Row {} of {}",
                                    format_count(at.get() + 1),
                                    format_count(rows)
                                )),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("record-previous")
                                .icon(Icon::new(IconName::ChevronUp))
                                .label("Previous")
                                .small()
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    go(&back.0, &back.1, -1, window, cx)
                                }),
                        )
                        .child(
                            Button::new("record-next")
                                .icon(Icon::new(IconName::ChevronDown))
                                .label("Next")
                                .small()
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    go(&next.0, &next.1, 1, window, cx)
                                }),
                        )
                        .child(
                            Button::new("dialog-close")
                                .label("Close")
                                .small()
                                .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                        ),
                )
        });
    }
}

/// Move to the row `delta` away, and select it in the grid too.
fn go(table: &ResultTable, at: &Rc<Cell<usize>>, delta: isize, window: &mut Window, cx: &mut App) {
    let rows = table.read(cx).delegate().view_len();
    if rows == 0 {
        return;
    }
    let row = at.get().saturating_add_signed(delta).min(rows - 1);
    at.set(row);
    table.update(cx, |table, cx| table.set_selected_row(row, cx));
    window.refresh();
}

/// The fields of grid row `row`, and how many rows the grid has.
fn fields(table: &ResultTable, row: usize, cx: &App) -> (Vec<impl IntoElement + use<>>, usize) {
    let delegate = table.read(cx).delegate();
    let rows = delegate.view_len();
    let (Some(result), Some(base)) = (delegate.result(), delegate.base_row(row)) else {
        return (Vec::new(), rows);
    };
    let theme = cx.theme();
    let (muted, mono, border) = (
        theme.muted_foreground,
        theme.mono_font_family.clone(),
        theme.border,
    );
    let fields = (0..result.column_count())
        .map(|col| {
            let value = result.cell_value(base, col);
            let shown = value.as_deref().map(|text| {
                let mut cut: String = text.chars().take(MAX_VALUE_CHARS).collect();
                if cut.len() < text.len() {
                    cut.push('…');
                }
                cut
            });
            let copy = value.clone().unwrap_or_default();
            h_flex()
                .id(("record-field", col))
                .items_start()
                .gap_3()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(
                    v_flex()
                        .w(px(200.))
                        .flex_shrink_0()
                        .child(
                            div()
                                .text_sm()
                                .truncate()
                                .child(result.column_name(col).to_owned()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .truncate()
                                .child(result.column_type_label(col)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .font_family(mono.clone())
                        .whitespace_normal()
                        .map_text(shown, muted),
                )
                .child(
                    Button::new(("record-copy", col))
                        .icon(Icon::new(IconName::Copy))
                        .xsmall()
                        .ghost()
                        .tooltip("Copy the value")
                        .on_click(move |_: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                )
        })
        .collect();
    (fields, rows)
}

trait MapText {
    fn map_text(self, text: Option<String>, muted: gpui_kit::Hsla) -> Self;
}

impl MapText for gpui_kit::Div {
    /// The value, or a muted NULL.
    fn map_text(self, text: Option<String>, muted: gpui_kit::Hsla) -> Self {
        match text {
            Some(text) if text.is_empty() => self.text_color(muted.opacity(0.7)).child("(empty)"),
            Some(text) => self.child(text),
            None => self.text_color(muted.opacity(0.7)).child("NULL"),
        }
    }
}
