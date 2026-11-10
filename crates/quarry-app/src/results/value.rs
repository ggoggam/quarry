//! The value viewer: the selected cell's whole value in a dialog, where the
//! grid shows one cut line of it.
//!
//! Text shows in a read-only editor with line numbers and soft wrap (⌘F
//! searches it); JSON (a `json`/`jsonb` column, or text that is a JSON
//! object or array) is pretty-printed and highlighted, with a switch back to
//! the text as stored; a blob shows as a hex dump. Formatting happens off
//! the UI thread. The editor holds at most [`MAX_SHOWN`] of a value; Copy
//! and Save Value to File always use all of it.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::Editor;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, HighlightStyle, IntoElement,
    ParentElement as _, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use quarry_core::arrow::array::Array as _;
use quarry_core::export::{binary_value, is_json_column};
use quarry_core::json;

use super::Results;
use super::grid::focused_cell;
use crate::format::format_count;
use crate::icons::{LucideIcon, icon};
use crate::ui;

/// The most of a value the viewer's editor holds, in bytes.
const MAX_SHOWN: usize = 2 * 1024 * 1024;
/// The most of a blob the hex dump shows, in bytes.
const MAX_HEX: usize = 64 * 1024;

/// A cell's value, taken out of the result.
#[derive(Clone)]
enum Value {
    Null,
    Text(Arc<str>),
    Binary(Arc<[u8]>),
}

struct Viewer {
    column: String,
    type_label: String,
    value: Value,
    /// The value as it is stored, and pretty-printed when it is JSON.
    raw: Option<String>,
    pretty: Option<String>,
    show_pretty: bool,
    editor: Option<Entity<EditorState>>,
    /// The file name Save offers.
    file_name: String,
}

impl Results {
    /// Show the selected cell's whole value (⇧↵, or a double click).
    pub fn view_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_result() else {
            return;
        };
        let table = active.table.read(cx);
        let delegate = table.delegate();
        let (Some((row, col)), Some(result)) = (focused_cell(table), delegate.result()) else {
            return;
        };
        let Some(base) = delegate.base_row(row) else {
            return;
        };
        let Some((array, local)) = result.column(base, col) else {
            return;
        };
        let field = result.schema().field(col);
        let json_column = is_json_column(field);
        let value = if array.is_null(local) {
            Value::Null
        } else if let Some(bytes) = binary_value(array.as_ref(), local) {
            Value::Binary(bytes.into())
        } else {
            Value::Text(result.cell_value(base, col).unwrap_or_default().into())
        };
        let extension = match &value {
            Value::Binary(_) => "bin",
            _ if json_column => "json",
            _ => "txt",
        };
        let viewer = Rc::new(RefCell::new(Viewer {
            column: field.name().clone(),
            type_label: result.column_type_label(col),
            file_name: format!("{}.{extension}", field.name()),
            value: value.clone(),
            raw: None,
            pretty: None,
            show_pretty: true,
            editor: None,
        }));
        // Lay the value out off the UI thread, then show it.
        let prepared = viewer.clone();
        cx.spawn_in(window, async move |_, cx| {
            let (raw, pretty) = cx
                .background_executor()
                .spawn(async move { prepare(&value, json_column) })
                .await;
            let _ = cx.update(|window, cx| {
                let shown = pretty.clone().or_else(|| raw.clone());
                let mut viewer = prepared.borrow_mut();
                viewer.raw = raw;
                viewer.pretty = pretty;
                if let Some(text) = shown {
                    let json = viewer.pretty.is_some();
                    viewer.editor = Some(cx.new(|cx| {
                        let mut editor = EditorState::new(window, cx)
                            .language(if json { "json" } else { "text" })
                            .line_number(true)
                            .soft_wrap(true)
                            .searchable(true)
                            .default_value(text);
                        if json {
                            editor.set_highlighter_factory(json_highlighter(), cx);
                        }
                        editor
                    }));
                }
                window.refresh();
            });
        })
        .detach();
        window.open_dialog(cx, move |dialog, _, cx| {
            let state = viewer.borrow();
            let theme = cx.theme();
            let muted = theme.muted_foreground;
            let about = about(&state);
            let body = match (&state.value, &state.editor) {
                (Value::Null, _) => div()
                    .h(px(200.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .child(ui::empty_state(
                        icon(LucideIcon::CircleSlash2),
                        "NULL",
                        Some(
                            "This cell holds no value, so there is nothing to copy or save.".into(),
                        ),
                        cx,
                    ))
                    .into_any_element(),
                (_, Some(editor)) => div()
                    .h(px(460.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .child(Editor::new(editor).bordered(false).readonly(true).h_full())
                    .into_any_element(),
                (_, None) => div()
                    .h(px(460.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(muted)
                    .child("Loading…")
                    .into_any_element(),
            };
            let has_value = !matches!(state.value, Value::Null);
            let can_format = state.pretty.is_some();
            let show_pretty = state.show_pretty;
            let (copy, save, toggle) = (viewer.clone(), viewer.clone(), viewer.clone());
            dialog
                .title(format!("{} · {}", state.column, state.type_label))
                .w(px(860.))
                .child(
                    v_flex()
                        .gap_2()
                        // A NULL says what it is in its own box.
                        .when(has_value, |this| {
                            this.child(div().text_xs().text_color(muted).child(about))
                        })
                        .child(body),
                )
                .footer(
                    DialogFooter::new()
                        .when_some_child(can_format.then(|| {
                            Button::new("value-format")
                                .label(if show_pretty {
                                    "Show as Stored"
                                } else {
                                    "Format JSON"
                                })
                                .ghost()
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    toggle_pretty(&toggle, window, cx)
                                })
                        }))
                        .child(div().flex_1())
                        .when_some_child(has_value.then(|| {
                            Button::new("value-copy")
                                .label("Copy")
                                .on_click(move |_: &ClickEvent, _, cx| copy_value(&copy, cx))
                        }))
                        .when_some_child(has_value.then(|| {
                            Button::new("value-save")
                                .label("Save Value to File…")
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    save_value(&save, window, cx)
                                })
                        }))
                        .child(
                            Button::new("dialog-close")
                                .label("Close")
                                .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                        ),
                )
        });
    }
}

/// `when_some` for one optional child, since a footer is not a `Div`.
trait WhenSomeChild: Sized {
    fn when_some_child(self, child: Option<impl IntoElement>) -> Self;
}

impl WhenSomeChild for DialogFooter {
    fn when_some_child(self, child: Option<impl IntoElement>) -> Self {
        match child {
            Some(child) => self.child(child),
            None => self,
        }
    }
}

/// A line about the value: its size, and how much of it shows.
fn about(viewer: &Viewer) -> String {
    match &viewer.value {
        Value::Null => "SQL NULL".into(),
        Value::Text(text) => {
            let chars = text.chars().count();
            let mut line = format!("{} characters", format_count(chars));
            if viewer.pretty.is_some() {
                line.push_str(if viewer.show_pretty {
                    " · JSON, formatted"
                } else {
                    " · JSON"
                });
            }
            let shown = if viewer.show_pretty {
                viewer.pretty.as_ref().or(viewer.raw.as_ref())
            } else {
                viewer.raw.as_ref()
            };
            if text.len() > MAX_SHOWN || shown.is_some_and(|s| s.len() >= MAX_SHOWN) {
                line.push_str(&format!(
                    " · showing the first {}; Copy and Save use all of it",
                    size(MAX_SHOWN)
                ));
            }
            line
        }
        Value::Binary(bytes) => {
            let mut line = format!("{} bytes", format_count(bytes.len()));
            if bytes.len() > MAX_HEX {
                line.push_str(&format!(
                    " · showing the first {}; Copy and Save use all of it",
                    size(MAX_HEX)
                ));
            }
            line
        }
    }
}

fn size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// The text to show for a value, cut to [`MAX_SHOWN`], and its JSON layout.
fn prepare(value: &Value, json_column: bool) -> (Option<String>, Option<String>) {
    match value {
        Value::Null => (None, None),
        Value::Binary(bytes) => (Some(hex_dump(&bytes[..bytes.len().min(MAX_HEX)])), None),
        Value::Text(text) => {
            let looks_like_json = json_column || json::is_json_document(text);
            let pretty = looks_like_json
                .then(|| json::pretty(text, 2))
                .flatten()
                .map(|p| cut(&p, MAX_SHOWN).to_owned());
            (Some(cut(text, MAX_SHOWN).to_owned()), pretty)
        }
    }
}

/// At most `max` bytes of `text`, on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `offset  hex bytes  |ascii|`, 16 bytes a line.
pub fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 4 + 16);
    for (line, chunk) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", line * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            out.push(if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

fn toggle_pretty(viewer: &Rc<RefCell<Viewer>>, window: &mut Window, cx: &mut App) {
    let mut state = viewer.borrow_mut();
    state.show_pretty = !state.show_pretty;
    let text = if state.show_pretty {
        state.pretty.clone()
    } else {
        state.raw.clone()
    };
    if let (Some(editor), Some(text)) = (state.editor.clone(), text) {
        drop(state);
        editor.update(cx, |editor, cx| editor.set_value(text, window, cx));
    }
    window.refresh();
}

/// The whole value as text: the JSON layout when it shows, hex for a blob.
fn full_text(viewer: &Viewer) -> Option<String> {
    match &viewer.value {
        Value::Null => None,
        Value::Text(text) => Some(
            if viewer.show_pretty && viewer.pretty.is_some() {
                json::pretty(text, 2)
            } else {
                None
            }
            .unwrap_or_else(|| text.to_string()),
        ),
        Value::Binary(bytes) => Some(bytes.iter().map(|b| format!("{b:02x}")).collect()),
    }
}

fn copy_value(viewer: &Rc<RefCell<Viewer>>, cx: &mut App) {
    if let Some(text) = full_text(&viewer.borrow()) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

/// Ask where to, then write the whole value there: bytes for a blob, the
/// text as stored otherwise.
fn save_value(viewer: &Rc<RefCell<Viewer>>, window: &mut Window, cx: &mut App) {
    let state = viewer.borrow();
    let bytes: Vec<u8> = match &state.value {
        Value::Null => return,
        Value::Text(text) => text.as_bytes().to_vec(),
        Value::Binary(bytes) => bytes.to_vec(),
    };
    let folder = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let path = cx.prompt_for_new_path(&folder, Some(&state.file_name));
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            let shown = path.display().to_string();
            let written = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, bytes) })
                .await;
            let _ = cx.update(|window, cx| {
                let message = match written {
                    Ok(()) => format!("Saved the value to {shown}"),
                    Err(err) => format!("Could not save to {shown}: {err}"),
                };
                window.push_notification(message, cx);
            });
        })
        .detach();
}

// ── JSON highlighting ────────────────────────────────────────────────────────

fn json_highlighter() -> InputHighlighterFactory {
    Rc::new(|_language| Some(Box::new(JsonHighlighter::default())))
}

#[derive(Default)]
struct JsonHighlighter {
    tokens: Vec<(Range<usize>, json::Token)>,
}

impl InputHighlighter for JsonHighlighter {
    fn language(&self) -> SharedString {
        "json".into()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        self.tokens = json::tokens(&text.to_string());
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let style = |token: json::Token| {
            let names: &[&str] = match token {
                json::Token::Key => &["property", "attribute", "function"],
                json::Token::String => &["string"],
                json::Token::Number => &["number"],
                json::Token::Literal => &["boolean", "constant", "keyword"],
                json::Token::Punctuation => &["punctuation.bracket", "punctuation"],
            };
            names
                .iter()
                .find_map(|name| resolver.style(name))
                .unwrap_or_default()
        };
        let mut runs = Vec::new();
        let mut at = range.start;
        let first = self.tokens.partition_point(|(r, _)| r.end <= range.start);
        for (span, token) in &self.tokens[first..] {
            if span.start >= range.end {
                break;
            }
            let (start, end) = (span.start.max(range.start), span.end.min(range.end));
            if start > at {
                runs.push((at..start, HighlightStyle::default()));
            }
            runs.push((start..end, style(*token)));
            at = end;
        }
        if at < range.end {
            runs.push((at..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_bytes_as_hex() {
        assert_eq!(
            hex_dump(b"Hello, world!\x00\x01\xff\nmore"),
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 ff  |Hello, world!...|\n\
             00000010  0a 6d 6f 72 65                                    |.more|\n"
        );
    }

    #[test]
    fn prepares_json_and_cuts_huge_values() {
        let (raw, pretty) = prepare(&Value::Text(r#"{"a":[1,2]}"#.into()), false);
        assert_eq!(raw.as_deref(), Some(r#"{"a":[1,2]}"#));
        assert_eq!(
            pretty.as_deref(),
            Some("{\n  \"a\": [\n    1,\n    2\n  ]\n}")
        );
        let (_, pretty) = prepare(&Value::Text("plain [text]".into()), false);
        assert!(pretty.is_none());
        let huge = "é".repeat(MAX_SHOWN);
        let (raw, _) = prepare(&Value::Text(huge.as_str().into()), false);
        let raw = raw.unwrap();
        assert!(raw.len() <= MAX_SHOWN && raw.len() > MAX_SHOWN - 2);
    }
}
