//! The result grid's actions: their shortcuts, the Result menu, the
//! context menus of cells and column headers, and the handlers that run
//! them on the result on screen.

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement, KeyBinding, MenuItem, Window,
};
use quarry_core::export::Format;

use super::Results;

gpui_kit::actions!(
    quarry,
    [
        CopyCells,
        CopyCellsWithHeaders,
        CopyAsCsv,
        CopyAsTsv,
        CopyAsJson,
        CopyAsMarkdown,
        CopyAsSqlInsert,
        SelectAllCells,
        ExtendSelectionUp,
        ExtendSelectionDown,
        ExtendSelectionLeft,
        ExtendSelectionRight,
        ExportCsv,
        ExportTsv,
        ExportJson,
        ExportMarkdown,
        ExportSqlInsert,
        CancelExport,
        SortAscending,
        SortDescending,
        AddSortAscending,
        AddSortDescending,
        ClearSort,
        FilterBySelection,
        FilterExcludingSelection,
        EditFilter,
        ClearFilter,
        FindInResult,
        FindNext,
        FindPrevious,
        UseSelectionForFind,
        CloseFind,
        ViewValue,
        ShowRecord,
    ]
);

/// The key context of a result grid (around the table).
pub const GRID_CONTEXT: &str = "ResultGrid";
/// The key context of the find bar.
pub const FIND_CONTEXT: &str = "ResultFind";
/// The key context of the filter bar.
pub const FILTER_CONTEXT: &str = "ResultFilter";

pub fn init(cx: &mut App) {
    let grid = Some(GRID_CONTEXT);
    cx.bind_keys([
        KeyBinding::new(crate::primary!("c"), CopyCells, grid),
        KeyBinding::new(crate::primary!("shift-c"), CopyCellsWithHeaders, grid),
        KeyBinding::new(crate::primary!("a"), SelectAllCells, grid),
        KeyBinding::new("shift-up", ExtendSelectionUp, grid),
        KeyBinding::new("shift-down", ExtendSelectionDown, grid),
        KeyBinding::new("shift-left", ExtendSelectionLeft, grid),
        KeyBinding::new("shift-right", ExtendSelectionRight, grid),
        KeyBinding::new(crate::primary!("f"), FindInResult, grid),
        KeyBinding::new(crate::primary!("g"), FindNext, grid),
        KeyBinding::new(crate::primary!("shift-g"), FindPrevious, grid),
        KeyBinding::new(crate::primary!("e"), UseSelectionForFind, grid),
        KeyBinding::new(crate::primary!("alt-f"), EditFilter, grid),
        KeyBinding::new("shift-enter", ViewValue, grid),
        KeyBinding::new(crate::primary!("i"), ShowRecord, grid),
    ]);
    // Inside the find bar's input, which binds keys of its own.
    let find = format!("{FIND_CONTEXT} > Input");
    let filter = format!("{FILTER_CONTEXT} > Input");
    cx.bind_keys([
        KeyBinding::new("escape", CloseFind, Some(find.as_str())),
        KeyBinding::new(crate::primary!("g"), FindNext, Some(find.as_str())),
        KeyBinding::new(
            crate::primary!("shift-g"),
            FindPrevious,
            Some(find.as_str()),
        ),
        KeyBinding::new(crate::primary!("f"), FindInResult, Some(find.as_str())),
        KeyBinding::new(crate::primary!("alt-f"), EditFilter, Some(filter.as_str())),
    ]);
}

fn copy_as_menu() -> MenuItem {
    MenuItem::submenu(gpui_kit::Menu {
        name: "Copy As".into(),
        items: vec![
            MenuItem::action("CSV", CopyAsCsv),
            MenuItem::action("TSV", CopyAsTsv),
            MenuItem::action("JSON", CopyAsJson),
            MenuItem::action("Markdown", CopyAsMarkdown),
            MenuItem::action("SQL INSERT", CopyAsSqlInsert),
        ],
        disabled: false,
    })
}

fn export_menu() -> MenuItem {
    MenuItem::submenu(gpui_kit::Menu {
        name: "Export Result".into(),
        items: vec![
            MenuItem::action("CSV…", ExportCsv),
            MenuItem::action("TSV…", ExportTsv),
            MenuItem::action("JSON…", ExportJson),
            MenuItem::action("Markdown…", ExportMarkdown),
            MenuItem::action("SQL INSERT…", ExportSqlInsert),
        ],
        disabled: false,
    })
}

/// The Result menu.
pub fn menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Copy Cells", CopyCells),
        MenuItem::action("Copy Cells with Headers", CopyCellsWithHeaders),
        copy_as_menu(),
        MenuItem::action("Select All Cells", SelectAllCells),
        MenuItem::separator(),
        export_menu(),
        MenuItem::action("Cancel Export", CancelExport),
        MenuItem::separator(),
        MenuItem::action("Sort Ascending", SortAscending),
        MenuItem::action("Sort Descending", SortDescending),
        MenuItem::action("Then Sort Ascending", AddSortAscending),
        MenuItem::action("Then Sort Descending", AddSortDescending),
        MenuItem::action("Clear Sort", ClearSort),
        MenuItem::separator(),
        MenuItem::action("Filter by Selected Value", FilterBySelection),
        MenuItem::action("Filter Excluding Selected Value", FilterExcludingSelection),
        MenuItem::action("Edit Filter", EditFilter),
        MenuItem::action("Clear Filter", ClearFilter),
        MenuItem::separator(),
        MenuItem::action("Find…", FindInResult),
        MenuItem::action("Find Next", FindNext),
        MenuItem::action("Find Previous", FindPrevious),
        MenuItem::action("Use Selection for Find", UseSelectionForFind),
        MenuItem::separator(),
        MenuItem::action("View Value", ViewValue),
        MenuItem::action("Record View", ShowRecord),
    ]
}

/// A cell's context menu. Its actions go to the grid, whose selection the
/// right click has just set.
pub fn cell_menu(
    menu: PopupMenu,
    focus: Option<FocusHandle>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let menu = match focus {
        Some(focus) => menu.action_context(focus),
        None => menu,
    };
    menu.menu("Copy", Box::new(CopyCells))
        .menu("Copy with Headers", Box::new(CopyCellsWithHeaders))
        .submenu("Copy As", window, cx, |menu, _, _| {
            menu.menu("CSV", Box::new(CopyAsCsv))
                .menu("TSV", Box::new(CopyAsTsv))
                .menu("JSON", Box::new(CopyAsJson))
                .menu("Markdown", Box::new(CopyAsMarkdown))
                .menu("SQL INSERT", Box::new(CopyAsSqlInsert))
        })
        .separator()
        .menu("Filter by This Value", Box::new(FilterBySelection))
        .menu(
            "Filter Excluding This Value",
            Box::new(FilterExcludingSelection),
        )
        .menu("Clear Filter", Box::new(ClearFilter))
        .separator()
        .menu("Sort Ascending", Box::new(SortAscending))
        .menu("Sort Descending", Box::new(SortDescending))
        .separator()
        .menu("Find…", Box::new(FindInResult))
        .menu("View Value", Box::new(ViewValue))
        .menu("Record View", Box::new(ShowRecord))
        .separator()
        .submenu("Export Result", window, cx, |menu, _, _| {
            menu.menu("CSV…", Box::new(ExportCsv))
                .menu("TSV…", Box::new(ExportTsv))
                .menu("JSON…", Box::new(ExportJson))
                .menu("Markdown…", Box::new(ExportMarkdown))
                .menu("SQL INSERT…", Box::new(ExportSqlInsert))
        })
}

/// A column header's context menu; the right click selected the column.
pub fn header_menu(
    menu: PopupMenu,
    _col: usize,
    focus: Option<FocusHandle>,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let menu = match focus {
        Some(focus) => menu.action_context(focus),
        None => menu,
    };
    menu.menu("Sort Ascending", Box::new(SortAscending))
        .menu("Sort Descending", Box::new(SortDescending))
        .menu("Then Sort Ascending", Box::new(AddSortAscending))
        .menu("Then Sort Descending", Box::new(AddSortDescending))
        .menu("Clear Sort", Box::new(ClearSort))
        .separator()
        .menu("Copy Column", Box::new(CopyCells))
        .menu("Copy Column with Header", Box::new(CopyCellsWithHeaders))
}

/// Handle the grid actions on `element`, the window's root, so they work
/// wherever the focus is (the menu bar's items included).
pub fn bind<E: InteractiveElement>(element: E, results: &Entity<Results>) -> E {
    macro_rules! on {
        ($element:expr, $action:ty, |$this:ident, $window:ident, $cx:ident| $body:expr) => {{
            let results = results.clone();
            $element.on_action(move |_: &$action, $window: &mut Window, cx: &mut App| {
                results.update(cx, |$this, $cx| $body)
            })
        }};
    }
    let e = element;
    let e = on!(e, CopyCells, |this, window, cx| this
        .copy_selection(None, false, window, cx));
    let e = on!(e, CopyCellsWithHeaders, |this, window, cx| this
        .copy_selection(None, true, window, cx));
    let e = on!(e, CopyAsCsv, |this, window, cx| this.copy_selection(
        Some(Format::Csv),
        true,
        window,
        cx
    ));
    let e = on!(e, CopyAsTsv, |this, window, cx| this.copy_selection(
        Some(Format::Tsv),
        true,
        window,
        cx
    ));
    let e = on!(e, CopyAsJson, |this, window, cx| this.copy_selection(
        Some(Format::Json),
        true,
        window,
        cx
    ));
    let e = on!(e, CopyAsMarkdown, |this, window, cx| this.copy_selection(
        Some(Format::Markdown),
        true,
        window,
        cx
    ));
    let e = on!(e, CopyAsSqlInsert, |this, window, cx| this.copy_selection(
        Some(Format::SqlInsert),
        true,
        window,
        cx
    ));
    let e = on!(e, SelectAllCells, |this, _window, cx| this
        .select_all_cells(cx));
    let e = on!(e, ExtendSelectionUp, |this, _window, cx| this
        .extend_selection(-1, 0, cx));
    let e = on!(e, ExtendSelectionDown, |this, _window, cx| this
        .extend_selection(1, 0, cx));
    let e = on!(e, ExtendSelectionLeft, |this, _window, cx| this
        .extend_selection(0, -1, cx));
    let e = on!(e, ExtendSelectionRight, |this, _window, cx| this
        .extend_selection(0, 1, cx));
    let e = on!(e, ExportCsv, |this, window, cx| this.export(
        Format::Csv,
        window,
        cx
    ));
    let e = on!(e, ExportTsv, |this, window, cx| this.export(
        Format::Tsv,
        window,
        cx
    ));
    let e = on!(e, ExportJson, |this, window, cx| this.export(
        Format::Json,
        window,
        cx
    ));
    let e = on!(e, ExportMarkdown, |this, window, cx| this.export(
        Format::Markdown,
        window,
        cx
    ));
    let e = on!(e, ExportSqlInsert, |this, window, cx| this.export(
        Format::SqlInsert,
        window,
        cx
    ));
    let e = on!(e, CancelExport, |this, _window, cx| this.cancel_export(cx));
    let e = on!(e, SortAscending, |this, _window, cx| this
        .sort_selected(false, false, cx));
    let e = on!(e, SortDescending, |this, _window, cx| this
        .sort_selected(true, false, cx));
    let e = on!(e, AddSortAscending, |this, _window, cx| this
        .sort_selected(false, true, cx));
    let e = on!(e, AddSortDescending, |this, _window, cx| this
        .sort_selected(true, true, cx));
    let e = on!(e, ClearSort, |this, _window, cx| this.clear_sort(cx));
    let e = on!(e, FilterBySelection, |this, window, cx| this
        .filter_by_selection(false, window, cx));
    let e = on!(e, FilterExcludingSelection, |this, window, cx| this
        .filter_by_selection(true, window, cx));
    let e = on!(e, EditFilter, |this, window, cx| this
        .edit_filter(window, cx));
    let e = on!(e, ClearFilter, |this, window, cx| this
        .clear_filter(window, cx));
    let e = on!(e, FindInResult, |this, window, cx| this
        .open_find(false, window, cx));
    let e = on!(e, UseSelectionForFind, |this, window, cx| this
        .open_find(true, window, cx));
    let e = on!(e, FindNext, |this, window, cx| this
        .find_step(true, window, cx));
    let e = on!(e, FindPrevious, |this, window, cx| this
        .find_step(false, window, cx));
    let e = on!(e, CloseFind, |this, window, cx| this.close_find(window, cx));
    let e = on!(e, ViewValue, |this, window, cx| this.view_value(window, cx));
    on!(e, ShowRecord, |this, window, cx| this
        .show_record(window, cx))
}
