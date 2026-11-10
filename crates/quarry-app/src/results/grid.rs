//! A result grid: a virtualized table over a columnar [`ResultSet`].
//!
//! The table asks for cells only in the visible range, and each cell is
//! formatted from its Arrow array at that moment, so scrolling a million-row
//! result costs the same as scrolling a hundred-row one.
//!
//! The grid shows the result through a view ([`arrange`](super::arrange)):
//! its rows sorted and filtered, as row indices. Row `i` of the grid is
//! result row `view[i]`, and the row number column counts grid rows, so
//! every tool that reads "the selected rows" goes through
//! [`ResultsDelegate::base_row`].

use std::ops::Range;

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::table::{Column, TableDelegate, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::{
    App, Context, FocusHandle, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px,
};
use quarry_core::ResultSet;
use quarry_core::arrow::datatypes::SchemaRef;
use quarry_core::result::{field_type_label, is_numeric};

use crate::icons::{LucideIcon, icon};

use super::Results;
use super::arrange::Arrange;
use super::commands;
use super::search::Search;

/// Column 0 of the grid is the row number; data column `i` is grid column
/// `i + 1`.
pub const ROW_NUMBER_COLUMNS: usize = 1;

/// A result tab's table.
pub type GridState = TableState<ResultsDelegate>;

/// Room around a cell's text. Columns have no padding of their own, so a
/// cell's highlight and its mouse handling cover all of it.
const CELL_PADDING_X: f32 = 6.;
const CELL_PADDING_Y: f32 = 3.;

#[derive(Default)]
pub struct ResultsDelegate {
    pub(super) result: Option<ResultSet>,
    /// Grid columns, row number first.
    columns: Vec<Column>,
    /// Type labels of the data columns.
    types: Vec<SharedString>,
    /// Memory the result's batches take, as Arrow counts it.
    bytes: usize,
    /// Sort, filter and the view of rows they make.
    pub(super) arrange: Arrange,
    /// Find in results.
    pub(super) search: Search,
    /// The selection beyond the table's one cell, row or column.
    pub(super) range: RangeState,
    /// Told when the view or a search changes, so the bars above the grid
    /// and the status bar redraw.
    pub(super) results: Option<WeakEntity<Results>>,
    /// The table's own focus, for the actions of its context menus.
    pub(super) focus: Option<FocusHandle>,
}

/// A rectangle of cells grown from an anchor, as shift-click and
/// shift-arrows make, or everything.
#[derive(Debug, Default, Clone)]
pub struct RangeState {
    /// The corner the range grows from: a grid row and grid column.
    pub anchor: Option<(usize, usize)>,
    /// The next selection change extends the range rather than starting over.
    pub extending: bool,
    /// Every cell (Select All).
    pub all: bool,
    /// The selected rectangle when it is more than one cell, for drawing:
    /// cells cannot read the table while it draws them.
    pub drawn: Option<CellRange>,
}

/// Selected cells: grid rows (positions in the view) and data columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRange {
    pub rows: Range<usize>,
    pub cols: Range<usize>,
}

impl CellRange {
    pub fn is_single_cell(&self) -> bool {
        self.rows.len() == 1 && self.cols.len() == 1
    }

    fn contains(&self, row: usize, col: usize) -> bool {
        self.rows.contains(&row) && self.cols.contains(&col)
    }
}

impl ResultsDelegate {
    pub fn new(results: WeakEntity<Results>) -> Self {
        Self {
            results: Some(results),
            ..Self::default()
        }
    }

    pub fn clear(&mut self) {
        self.result = None;
        self.columns.clear();
        self.types.clear();
        self.bytes = 0;
        self.arrange.reset(false);
        self.search.reset();
        self.range = RangeState::default();
    }

    /// The result the grid shows, still arriving while its query runs.
    pub fn result(&self) -> Option<&ResultSet> {
        self.result.as_ref()
    }

    pub fn set_schema(&mut self, schema: SchemaRef) {
        // A rerun keeps its sort and filter when its columns are the same.
        let names = |s: &SchemaRef| -> Vec<String> {
            s.fields().iter().map(|f| f.name().clone()).collect()
        };
        let same_columns = self
            .result
            .as_ref()
            .is_some_and(|old| names(old.schema()) == names(&schema));
        let row_number = Column::new("#", "#")
            .width(px(64.))
            .text_right()
            .fixed_left()
            .resizable(false)
            .movable(false)
            .selectable(false)
            .p_0();
        self.types = schema
            .fields()
            .iter()
            .map(|f| field_type_label(f).into())
            .collect();
        self.columns = std::iter::once(row_number)
            .chain(schema.fields().iter().enumerate().map(|(ix, field)| {
                let data_type = field.data_type();
                let width = initial_width(field.name(), &self.types[ix], data_type);
                let column =
                    Column::new(SharedString::from(format!("c{ix}")), field.name().clone())
                        .width(px(width))
                        .min_width(px(48.))
                        .p_0();
                if is_numeric(data_type) {
                    column.text_right()
                } else {
                    column
                }
            }))
            .collect();
        self.result = Some(ResultSet::new(schema));
        self.bytes = 0;
        self.arrange.reset(same_columns);
        self.search.reset();
        self.range = RangeState::default();
    }

    pub fn push(&mut self, batch: quarry_core::arrow::record_batch::RecordBatch) {
        if let Some(result) = &mut self.result {
            self.bytes += batch.get_array_memory_size();
            result.push(batch);
        }
    }

    /// Rows in the result, whatever the view shows.
    pub fn row_count(&self) -> usize {
        self.result.as_ref().map_or(0, ResultSet::row_count)
    }

    /// Rows the grid shows: the view's, or all of them.
    pub fn view_len(&self) -> usize {
        match &self.arrange.view {
            Some(view) => view.len(),
            None => self.row_count(),
        }
    }

    /// The result row at grid row `row`.
    pub fn base_row(&self, row: usize) -> Option<usize> {
        match &self.arrange.view {
            Some(view) => view.get(row).map(|&r| r as usize),
            None => (row < self.row_count()).then_some(row),
        }
    }

    /// Memory the result takes, roughly: what its Arrow buffers hold.
    pub fn memory_bytes(&self) -> usize {
        self.bytes
    }

    pub fn data_columns(&self) -> usize {
        self.types.len()
    }

    /// Tell the results pane its bars need drawing again.
    pub(super) fn changed(&self, cx: &mut Context<GridState>) {
        cx.notify();
        if let Some(results) = self.results.clone() {
            cx.defer(move |cx| {
                let _ = results.update(cx, |_, cx| cx.notify());
            });
        }
    }
}

/// The selected cells, as grid rows and data columns. A selected row, or a
/// cell of the row number column, is all of its row; a selected column is
/// all of it.
pub fn selected_range(table: &GridState) -> Option<CellRange> {
    let delegate = table.delegate();
    let (rows, cols) = (delegate.view_len(), delegate.data_columns());
    if rows == 0 || cols == 0 {
        return None;
    }
    let all_cols = 0..cols;
    if delegate.range.all {
        return Some(CellRange {
            rows: 0..rows,
            cols: all_cols,
        });
    }
    let span = |a: usize, b: usize| a.min(b)..a.max(b) + 1;
    let range = match table.selection() {
        TableSelection::None => return None,
        TableSelection::Row(row) => match delegate.range.anchor {
            Some((anchor, _)) => CellRange {
                rows: span(anchor, row),
                cols: all_cols,
            },
            None => CellRange {
                rows: row..row + 1,
                cols: all_cols,
            },
        },
        TableSelection::Column(col) => CellRange {
            rows: 0..rows,
            cols: match col.checked_sub(ROW_NUMBER_COLUMNS) {
                Some(col) => col..col + 1,
                None => all_cols,
            },
        },
        TableSelection::Cell(row, col) => {
            let (anchor_row, anchor_col) = delegate.range.anchor.unwrap_or((row, col));
            let rows = span(anchor_row, row);
            // The row number column stands for whole rows.
            let cols = match (
                anchor_col.checked_sub(ROW_NUMBER_COLUMNS),
                col.checked_sub(ROW_NUMBER_COLUMNS),
            ) {
                (Some(a), Some(b)) => span(a, b),
                _ => all_cols,
            };
            CellRange { rows, cols }
        }
    };
    Some(CellRange {
        rows: range.rows.start.min(rows)..range.rows.end.min(rows),
        cols: range.cols.start.min(cols)..range.cols.end.min(cols),
    })
}

/// The one cell the selection is about (where it was clicked last): a grid
/// row and a data column. A row's first column stands for the row.
pub fn focused_cell(table: &GridState) -> Option<(usize, usize)> {
    match table.selection() {
        TableSelection::Cell(row, col) => Some((row, col.saturating_sub(ROW_NUMBER_COLUMNS))),
        TableSelection::Row(row) => Some((row, 0)),
        TableSelection::Column(col) => Some((0, col.checked_sub(ROW_NUMBER_COLUMNS)?)),
        TableSelection::None => None,
    }
    .filter(|&(row, col)| {
        row < table.delegate().view_len() && col < table.delegate().data_columns()
    })
}

/// The widest a column starts.
const MAX_INITIAL_WIDTH: f32 = 360.;

/// A first guess at a column's width, before any data arrives: room for its
/// header (the name, the type label and the sort control) and for a typical
/// value of its type. Users resize from there.
fn initial_width(
    name: &str,
    type_label: &str,
    data_type: &quarry_core::arrow::datatypes::DataType,
) -> f32 {
    use quarry_core::arrow::datatypes::DataType::*;
    let by_type: f32 = match data_type {
        Boolean => 72.,
        Int8 | Int16 | UInt8 | UInt16 => 72.,
        Int32 | UInt32 | Float32 => 96.,
        Int64 | UInt64 | Float64 | Decimal128(..) | Decimal256(..) => 120.,
        Date32 | Date64 => 104.,
        Timestamp(..) => 184.,
        Utf8 | LargeUtf8 | Utf8View => 200.,
        _ => 160.,
    };
    by_type
        .max(header_width(name, type_label))
        .min(MAX_INITIAL_WIDTH)
}

/// Roughly how wide a header is with nothing cut: the name at the header's
/// size, the type label at the smaller one, the sort control (an arrow and
/// a key number), the gaps between them and the padding.
fn header_width(name: &str, type_label: &str) -> f32 {
    const NAME_CHAR: f32 = 8.;
    const TYPE_CHAR: f32 = 7.;
    const SORT_CONTROL: f32 = 24.;
    const GAPS: f32 = 2. * HEADER_GAP;
    let chars = |s: &str| s.chars().count() as f32;
    chars(name) * NAME_CHAR
        + chars(type_label) * TYPE_CHAR
        + SORT_CONTROL
        + GAPS
        + 2. * CELL_PADDING_X
}

/// Room between a header's name, type label and sort control.
const HEADER_GAP: f32 = 6.;

/// How much faster a header's type label gives way than its name: the type
/// is cut (to nothing) before the name loses a letter.
const TYPE_SHRINK: f32 = 1000.;

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.view_len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<GridState>,
    ) -> impl IntoElement {
        let header = h_flex()
            .id(("result-th", col_ix))
            .size_full()
            .px(px(CELL_PADDING_X))
            .gap(px(HEADER_GAP))
            .overflow_hidden();
        let Some(data_col) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return header
                .justify_end()
                .text_color(cx.theme().muted_foreground)
                .child("#")
                .into_any_element();
        };
        let column = &self.columns[col_ix];
        let sorted = self
            .arrange
            .sort
            .iter()
            .position(|k| k.column == data_col)
            .map(|at| (at, self.arrange.sort[at].descending));
        let many = self.arrange.sort.len() > 1;
        let group = SharedString::from(format!("result-th-{col_ix}"));
        let muted = cx.theme().muted_foreground;
        let sort_button = h_flex()
            .id(("result-sort", col_ix))
            .flex_shrink_0()
            .ml_auto()
            .px_0p5()
            .gap_0p5()
            .rounded(cx.theme().radius / 2.)
            .cursor_pointer()
            .text_xs()
            .text_color(muted)
            .hover(|style| style.bg(cx.theme().secondary))
            .map(|this| match sorted {
                Some((at, descending)) => this
                    .text_color(cx.theme().foreground)
                    .child(
                        icon(if descending {
                            LucideIcon::ArrowDown
                        } else {
                            LucideIcon::ArrowUp
                        })
                        .size_3(),
                    )
                    .when(many, |this| this.child((at + 1).to_string())),
                None => this
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
                    .child(icon(LucideIcon::ChevronsUpDown).size_3()),
            })
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(
                    "Sort by this column (⇧-click to add it to the sort)",
                )
                .build(window, cx)
            })
            .on_click(
                cx.listener(move |table, event: &gpui_kit::ClickEvent, _, cx| {
                    cx.stop_propagation();
                    let add = event.modifiers().shift || event.modifiers().alt;
                    super::arrange::toggle_sort(table, data_col, add, cx);
                }),
            );
        let focus = self.focus.clone();
        // Short of room, the type label goes first, then the name is cut;
        // the sort control always shows.
        let type_label = div()
            .min_w_0()
            .text_xs()
            .text_color(muted)
            .truncate()
            .child(self.types[data_col].clone())
            .flex_shrink(TYPE_SHRINK);
        let name = div()
            .min_w_0()
            .flex_shrink_1()
            .truncate()
            .text_color(cx.theme().foreground)
            .child(column.name.clone());
        header
            .group(group)
            .child(
                // On one baseline, though the type label is smaller.
                h_flex()
                    .items_baseline()
                    .min_w_0()
                    .flex_shrink_1()
                    .gap(px(HEADER_GAP))
                    .child(name)
                    .child(type_label),
            )
            .child(sort_button)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _: &MouseDownEvent, _, cx| {
                    if table.selected_col() != Some(col_ix) {
                        table.set_selected_col(col_ix, cx);
                    }
                }),
            )
            .context_menu(move |menu, window, cx| {
                commands::header_menu(menu, data_col, focus.clone(), window, cx)
            })
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<GridState>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let cols = self.columns.len().max(1);
        let cell = div()
            .id(("result-td", row_ix * cols + col_ix))
            .size_full()
            .px(px(CELL_PADDING_X))
            .py(px(CELL_PADDING_Y))
            .truncate()
            .font_family(theme.mono_font_family.clone());
        let data_col = col_ix.checked_sub(ROW_NUMBER_COLUMNS);
        let text = match data_col {
            None => Some((row_ix + 1).to_string()),
            Some(col) => self.base_row(row_ix).and_then(|row| {
                self.result
                    .as_ref()
                    .and_then(|result| result.cell_text(row, col))
            }),
        };
        let in_range = data_col.is_some_and(|col| {
            self.range
                .drawn
                .as_ref()
                .is_some_and(|range| range.contains(row_ix, col))
        });
        let found = data_col.and_then(|col| self.search.at(row_ix, col));
        let cell = match (found, in_range) {
            (Some(true), _) => cell.bg(theme.warning.opacity(0.55)),
            (Some(false), _) => cell.bg(theme.warning.opacity(0.22)),
            (None, true) => cell.bg(theme.table_active),
            (None, false) => cell,
        };
        let cell = match (data_col, text) {
            // The row numbers sit on the header's surface, a step off the rows.
            (None, Some(text)) => cell
                .bg(theme.table_head)
                .text_color(theme.muted_foreground)
                .child(text),
            (_, Some(text)) => cell.child(text),
            (_, None) => cell
                .text_color(theme.muted_foreground.opacity(0.7))
                .child("NULL"),
        };
        let focus = self.focus.clone();
        cell.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |table, event: &MouseDownEvent, _, _| {
                let current = match table.selection() {
                    TableSelection::Cell(row, col) => Some((row, col)),
                    TableSelection::Row(row) => Some((row, 0)),
                    _ => None,
                };
                let range = &mut table.delegate_mut().range;
                if event.modifiers.shift && current.is_some() {
                    range.anchor = range.anchor.or(current);
                    range.extending = true;
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |table, _: &MouseDownEvent, _, cx| {
                // A right click outside the selection selects the cell, so
                // the menu acts on what was clicked.
                let inside = selected_range(table).is_some_and(|range| match data_col {
                    Some(col) => range.contains(row_ix, col),
                    None => range.rows.contains(&row_ix),
                });
                if !inside {
                    table.set_selected_cell(row_ix, col_ix, cx);
                }
            }),
        )
        .context_menu(move |menu, window, cx| commands::cell_menu(menu, focus.clone(), window, cx))
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let result = self.result.as_ref()?;
        let Some(col_ix) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return Some((row_ix + 1).to_string().into());
        };
        let row = self.base_row(row_ix)?;
        Some(
            result
                .cell_text(row, col_ix)
                .unwrap_or_else(|| "NULL".into())
                .into(),
        )
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let Some(col_ix) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return (row_ix + 1).to_string();
        };
        self.base_row(row_ix)
            .and_then(|row| self.result.as_ref()?.cell_value(row, col_ix))
            .unwrap_or_default()
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<GridState>) -> impl IntoElement {
        let (glyph, message) = match (&self.result, self.arrange.filter.is_some()) {
            (Some(_), true) => (LucideIcon::ListFilter, "No rows match the filter"),
            (Some(_), false) => (LucideIcon::Table, "No rows"),
            (None, _) => (LucideIcon::Play, "Run a query to see results"),
        };
        crate::ui::empty_state(icon(glyph), message, None, cx)
    }
}

/// Note what the selection now covers, for drawing; call after it changes.
pub fn refresh_range(table: &mut GridState) {
    let range = selected_range(table).filter(|r| !r.is_single_cell());
    table.delegate_mut().range.drawn = range;
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarry_core::arrow::datatypes::DataType;

    #[test]
    fn columns_start_wide_enough_for_their_header() {
        // A short name with a long type used to cut the type at once.
        let id = initial_width("id", "integer", &DataType::Int32);
        assert!(id >= header_width("id", "integer"));
        let paid = initial_width("paid", "boolean", &DataType::Boolean);
        assert!(paid >= header_width("paid", "boolean"));
        // Values still decide when they need more than the header.
        assert_eq!(initial_width("n", "bigint", &DataType::Int64), 120.);
        // And nothing starts wider than the cap.
        let long = "a_very_long_column_name_that_goes_on_and_on";
        assert_eq!(
            initial_width(long, "varchar", &DataType::Utf8),
            MAX_INITIAL_WIDTH
        );
    }
}
