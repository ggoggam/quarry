//! Copying a result grid's cells to the clipboard and exporting a whole
//! result to a file, through [`quarry_core::export`].
//!
//! Both go through the grid's view: the rows as sorted and filtered, with
//! the values in full. ⌘C copies one cell as its plain value and a range as
//! TSV (without headers, the way spreadsheets paste it); Copy As picks a
//! format, with headers where it has them. A large copy is formatted off the
//! UI thread. A file export streams to a temporary file next to the target
//! on its own thread, which takes its place once complete, so cancelling or
//! failing never leaves half a file behind.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::{
    ClickEvent, ClipboardItem, Context, IntoElement, ParentElement as _, Styled as _, Task, Window,
    div,
};
use quarry_core::ResultSet;
use quarry_core::export::{self, Format, Options};

use super::grid::{GridState, selected_range};
use super::{ResultData, ResultState, ResultTabId, Results, title};
use crate::format::{format_count, plural};

/// Copies of more cells than this are formatted off the UI thread.
const SYNC_COPY_CELLS: usize = 20_000;

/// A file export under way.
pub struct ExportJob {
    pub tab: ResultTabId,
    pub path: PathBuf,
    pub format: Format,
    pub total: usize,
    written: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    started: Instant,
    _task: Task<()>,
}

/// What to write: rows of the view, by result row, and columns.
struct Selection {
    result: ResultSet,
    view: Option<Arc<[u32]>>,
    rows: std::ops::Range<usize>,
    cols: Vec<usize>,
}

impl Selection {
    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.clone().map(|at| match &self.view {
            Some(view) => view[at] as usize,
            None => at,
        })
    }
}

/// The whole view of `table`, every column.
fn everything(table: &GridState) -> Option<Selection> {
    let delegate = table.delegate();
    let result = delegate.result()?.clone();
    Some(Selection {
        cols: (0..result.column_count()).collect(),
        rows: 0..delegate.view_len(),
        view: delegate.arrange.view.clone(),
        result,
    })
}

impl Results {
    /// The SQL target for INSERTs from a result: the table its statement
    /// reads, else `result`.
    fn insert_options(data: &ResultData, headers: bool) -> Options {
        Options {
            headers,
            dialect: data.dialect,
            table: title::source_table(&data.statement, data.dialect),
        }
    }

    /// Copy the selected cells: as `format`, or with `None` as plain text
    /// (one cell's value, or TSV for more).
    pub fn copy_selection(
        &mut self,
        format: Option<Format>,
        headers: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_id() else {
            return;
        };
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let table = tab.data.table.read(cx);
        let selected = selected_range(table);
        let selection = match (&selected, format) {
            (Some(range), _) => {
                let delegate = table.delegate();
                delegate.result().map(|result| Selection {
                    result: result.clone(),
                    view: delegate.arrange.view.clone(),
                    rows: range.rows.clone(),
                    cols: range.cols.clone().collect(),
                })
            }
            // Copy As with nothing selected copies the whole result.
            (None, Some(_)) => everything(table),
            (None, None) => None,
        };
        let Some(selection) = selection else {
            return;
        };
        // One cell, copied plainly, is its value as it is.
        if format.is_none() && !headers && selected.as_ref().is_some_and(|r| r.is_single_cell()) {
            let value = selection
                .rows()
                .next()
                .and_then(|row| selection.result.cell_value(row, selection.cols[0]))
                .unwrap_or_default();
            cx.write_to_clipboard(ClipboardItem::new_string(value));
            return;
        }
        let format = format.unwrap_or(Format::Tsv);
        let options = Self::insert_options(&tab.data, headers);
        let cells = selection.rows.len() * selection.cols.len();
        let rows = selection.rows.len();
        let write = move || {
            export::to_string(
                &selection.result,
                selection.rows(),
                &selection.cols,
                format,
                options,
            )
        };
        if cells <= SYNC_COPY_CELLS {
            cx.write_to_clipboard(ClipboardItem::new_string(write()));
            return;
        }
        window.push_notification(
            format!("Copying {} as {}…", plural(rows, "row"), format.label()),
            cx,
        );
        cx.spawn_in(window, async move |_, cx| {
            let text = cx.background_executor().spawn(async move { write() }).await;
            let bytes = text.len();
            let _ = cx.update(|window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                window.push_notification(
                    format!(
                        "Copied {} as {} ({})",
                        plural(rows, "row"),
                        format.label(),
                        size(bytes)
                    ),
                    cx,
                );
            });
        })
        .detach();
    }

    // ── Export ───────────────────────────────────────────────────────────────

    /// The folder the save dialog starts in: the last export's, else the
    /// active SQL file's, else Downloads.
    fn export_folder(&self) -> PathBuf {
        self.export_dir
            .clone()
            .or_else(|| self.file_dir.clone())
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Ask where to, then export the result on screen as `format`.
    pub fn export(&mut self, format: Format, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_result() else {
            return;
        };
        let id = active.id;
        if matches!(active.state, ResultState::Released { .. }) || active.result_set(cx).is_none() {
            window.push_notification("Run the statement again to export its result", cx);
            return;
        }
        if let Some(job) = &self.export {
            window.push_notification(format!("Already exporting to {}", job.path.display()), cx);
            return;
        }
        let name = format!(
            "{}.{}",
            file_stem(self.tabs.title(id).unwrap_or("result")),
            format.extension()
        );
        let path = cx.prompt_for_new_path(&self.export_folder(), Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.start_export(id, path, format, window, cx)
            });
        })
        .detach();
    }

    fn start_export(
        &mut self,
        id: ResultTabId,
        path: PathBuf,
        format: Format,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(id) else {
            return;
        };
        let Some(selection) = everything(tab.data.table.read(cx)) else {
            window.push_notification("Run the statement again to export its result", cx);
            return;
        };
        let options = Self::insert_options(&tab.data, true);
        let total = selection.rows.len();
        let written = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = flume::bounded(1);
        {
            let (written, stop, path) = (written.clone(), stop.clone(), path.clone());
            std::thread::Builder::new()
                .name("export".into())
                .spawn(move || {
                    let outcome = write_file(&path, &selection, format, options, &written, &stop);
                    let _ = done_tx.send(outcome);
                })
                .expect("the export thread starts");
        }
        self.export_dir = path.parent().map(Path::to_path_buf);
        let task = cx.spawn_in(window, async move |this, cx| {
            // Redraw the progress now and then until it is done.
            let outcome = loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                match done_rx.try_recv() {
                    Ok(outcome) => break outcome,
                    Err(flume::TryRecvError::Disconnected) => {
                        break Err(std::io::Error::other("the export stopped"));
                    }
                    Err(flume::TryRecvError::Empty) => {}
                }
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(job) = this.export.take() else {
                    return;
                };
                let message = match outcome {
                    Ok(rows) => format!(
                        "Exported {} to {} in {}",
                        plural(rows, "row"),
                        job.path.display(),
                        crate::format::format_elapsed(job.started.elapsed())
                    ),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                        "Export cancelled".to_owned()
                    }
                    Err(err) => format!("Could not export to {}: {err}", job.path.display()),
                };
                window.push_notification(message, cx);
                cx.notify();
            });
        });
        self.export = Some(ExportJob {
            tab: id,
            path,
            format,
            total,
            written,
            stop,
            started: Instant::now(),
            _task: task,
        });
        cx.notify();
    }

    pub fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.export {
            job.stop.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// The progress of an export of tab `id`, with a way to cancel it.
    pub(super) fn render_export_progress(
        &self,
        id: ResultTabId,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let job = self.export.as_ref().filter(|job| job.tab == id)?;
        let written = job.written.load(Ordering::Relaxed);
        let percent = written as f64 * 100.0 / job.total.max(1) as f64;
        let name = job
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(
            crate::ui::bar(cx)
                .gap_2()
                .text_xs()
                .child(div().flex_1().truncate().child(format!(
                    "Exporting {} to {name}… {} of {} rows ({percent:.0}%)",
                    job.format.label(),
                    format_count(written),
                    format_count(job.total),
                )))
                .child(
                    Button::new("cancel-export")
                        .label("Cancel")
                        .xsmall()
                        .ghost()
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_export(cx)),
                        ),
                ),
        )
    }
}

/// Write the selection to a temporary file next to `path`, then put it in
/// `path`'s place.
fn write_file(
    path: &Path,
    selection: &Selection,
    format: Format,
    options: Options,
    written: &AtomicUsize,
    stop: &AtomicBool,
) -> std::io::Result<usize> {
    let folder = path.parent().filter(|p| !p.as_os_str().is_empty());
    let mut builder = tempfile::Builder::new();
    builder.prefix(".quarry-export-");
    // A temporary file is private to its owner; the export should get the
    // permissions any new file gets (the umask applies).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut file = builder.tempfile_in(folder.unwrap_or(Path::new(".")))?;
    let rows = {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, file.as_file_mut());
        export::write(
            &selection.result,
            selection.rows(),
            &selection.cols,
            format,
            options,
            &mut out,
            &mut |rows| {
                written.store(rows, Ordering::Relaxed);
                !stop.load(Ordering::Relaxed)
            },
        )?
    };
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(rows)
}

/// A tab title as a file name: no path separators or other characters
/// that file systems refuse.
fn file_stem(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_matches('.').to_owned();
    if stem.is_empty() {
        "result".into()
    } else {
        stem
    }
}

/// `1.2 MB`.
fn size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b => format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_files_after_tabs() {
        assert_eq!(file_stem("orders"), "orders");
        assert_eq!(file_stem("sales.parquet"), "sales.parquet");
        assert_eq!(file_stem("a/b: c?"), "a_b_ c_");
        assert_eq!(file_stem(" .. "), "result");
        assert_eq!(size(10), "10 bytes");
        assert_eq!(size(3 * 1024 * 1024 + 1), "3.0 MB");
    }
}
