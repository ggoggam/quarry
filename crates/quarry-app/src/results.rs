//! The result grid: a virtualized table over a columnar [`ResultSet`].
//!
//! The table asks for cells only in the visible range, and each cell is
//! formatted from its Arrow array at that moment, so scrolling a million-row
//! result costs the same as scrolling a hundred-row one.

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    App, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use quarry_core::ResultSet;
use quarry_core::arrow::datatypes::SchemaRef;
use quarry_core::result::{field_type_label, is_numeric};

/// Column 0 of the grid is the row number; data column `i` is grid column
/// `i + 1`.
const ROW_NUMBER_COLUMNS: usize = 1;

#[derive(Default)]
pub struct ResultsDelegate {
    result: Option<ResultSet>,
    /// Grid columns, row number first.
    columns: Vec<Column>,
    /// Type labels of the data columns.
    types: Vec<SharedString>,
}

impl ResultsDelegate {
    pub fn clear(&mut self) {
        self.result = None;
        self.columns.clear();
        self.types.clear();
    }

    pub fn set_schema(&mut self, schema: SchemaRef) {
        let row_number = Column::new("#", "#")
            .width(px(64.))
            .text_right()
            .fixed_left()
            .resizable(false)
            .movable(false)
            .selectable(false);
        self.columns = std::iter::once(row_number)
            .chain(schema.fields().iter().enumerate().map(|(ix, field)| {
                let data_type = field.data_type();
                let column =
                    Column::new(SharedString::from(format!("c{ix}")), field.name().clone())
                        .width(px(initial_width(field.name(), data_type)))
                        .min_width(px(48.));
                if is_numeric(data_type) {
                    column.text_right()
                } else {
                    column
                }
            }))
            .collect();
        self.types = schema
            .fields()
            .iter()
            .map(|f| field_type_label(f).into())
            .collect();
        self.result = Some(ResultSet::new(schema));
    }

    pub fn push(&mut self, batch: quarry_core::arrow::record_batch::RecordBatch) {
        if let Some(result) = &mut self.result {
            result.push(batch);
        }
    }

    pub fn row_count(&self) -> usize {
        self.result.as_ref().map_or(0, ResultSet::row_count)
    }
}

/// A first guess at a column's width from its name and type, before any
/// data arrives. Users resize from there.
fn initial_width(name: &str, data_type: &quarry_core::arrow::datatypes::DataType) -> f32 {
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
    let by_name = name.chars().count() as f32 * 8.0 + 56.0;
    by_type.max(by_name).min(360.)
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.row_count()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let column = &self.columns[col_ix];
        let Some(data_col) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return h_flex()
                .w_full()
                .justify_end()
                .text_color(cx.theme().muted_foreground)
                .child("#");
        };
        h_flex()
            .w_full()
            .gap_1p5()
            .overflow_hidden()
            .child(div().flex_shrink_0().child(column.name.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(self.types[data_col].clone()),
            )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(data_col) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return div()
                .w_full()
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(cx.theme().muted_foreground)
                .child((row_ix + 1).to_string());
        };
        let text = self
            .result
            .as_ref()
            .and_then(|result| result.cell_text(row_ix, data_col));
        let cell = div()
            .w_full()
            .truncate()
            .font_family(cx.theme().mono_font_family.clone());
        match text {
            Some(text) => cell.child(text),
            None => cell
                .text_color(cx.theme().muted_foreground.opacity(0.7))
                .child("NULL"),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        let result = self.result.as_ref()?;
        let Some(col_ix) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return Some((row_ix + 1).to_string().into());
        };
        Some(
            result
                .cell_text(row_ix, col_ix)
                .unwrap_or_else(|| "NULL".into())
                .into(),
        )
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        let Some(col_ix) = col_ix.checked_sub(ROW_NUMBER_COLUMNS) else {
            return (row_ix + 1).to_string();
        };
        self.result
            .as_ref()
            .and_then(|result| result.cell_value(row_ix, col_ix))
            .unwrap_or_default()
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let message = if self.result.is_some() {
            "No rows"
        } else {
            "Run a query to see results"
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }
}
