//! Completion in the SQL editor: the input's own completion menu, fed by
//! [`quarry_core::complete`].
//!
//! The menu is gpui-component's, which speaks LSP: it asks a
//! [`CompletionProvider`] for items on each keystroke and on ctrl-space,
//! and inserts the chosen one. This adapter only translates. Each tab has
//! its own provider, holding the flavor the tab writes; the catalog index is
//! shared by every tab, as all the tabs shown run on the window's
//! connection, and is rebuilt only when the catalog is read again.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::base::input::{Rope, RopeExt as _};
use gpui_kit::component::input::{CompletionProvider, EditorState, highlight_characters};
use gpui_kit::{App, Task, Window, px};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, TextEdit,
};
use quarry_core::Flavor;
use quarry_core::complete::{self, CatalogIndex, ItemKind, Trigger};

/// The catalog every tab completes against.
pub type SharedCatalog = Rc<RefCell<CatalogIndex>>;

/// One tab's completion provider.
pub struct Completer {
    catalog: SharedCatalog,
    flavor: RefCell<Flavor>,
}

impl Completer {
    /// Complete in `editor` with names from `catalog` and the words of
    /// `flavor`.
    pub fn install(
        editor: &mut EditorState,
        catalog: SharedCatalog,
        flavor: Flavor,
    ) -> Rc<Completer> {
        let completer = Rc::new(Completer {
            catalog,
            flavor: RefCell::new(flavor),
        });
        let lsp = editor.lsp_mut();
        lsp.completion_provider = Some(completer.clone());
        // Room for a column's type and table next to its name.
        lsp.completion_menu.max_width = px(440.);
        lsp.set_completion_highlighter(Rc::new(|query, item| {
            highlight_characters(typed_word(query), item)
        }));
        completer
    }

    /// The tab now writes `flavor`.
    pub fn set_flavor(&self, flavor: &Flavor) {
        if *self.flavor.borrow() != *flavor {
            *self.flavor.borrow_mut() = flavor.clone();
        }
    }
}

impl CompletionProvider for Completer {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        context: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let trigger = if context.trigger_kind == CompletionTriggerKind::INVOKED {
            Trigger::Invoked
        } else {
            Trigger::Typing
        };
        let sql = text.to_string();
        let completions = complete::complete(
            &sql,
            offset,
            &self.flavor.borrow(),
            &self.catalog.borrow(),
            trigger,
        );
        let range = lsp_range(text, &completions.replace);
        let items = completions
            .items
            .into_iter()
            .map(|item| CompletionItem {
                kind: Some(lsp_kind(item.kind)),
                detail: Some(item.detail),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: item.insert,
                })),
                label: item.label,
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    /// Typing one character of a name, a `.` after a qualifier, or a quote
    /// opening a name asks for completions; the core decides whether there
    /// are any (none in a string, say).
    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        let mut chars = new_text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => {
                c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '"' | '`' | '[')
            }
            _ => false,
        }
    }
}

fn lsp_range(text: &Rope, range: &Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: text.offset_to_position(range.start),
        end: text.offset_to_position(range.end),
    }
}

fn lsp_kind(kind: ItemKind) -> CompletionItemKind {
    match kind {
        ItemKind::Database | ItemKind::Schema => CompletionItemKind::MODULE,
        ItemKind::Table | ItemKind::Cte => CompletionItemKind::STRUCT,
        ItemKind::View | ItemKind::MaterializedView => CompletionItemKind::INTERFACE,
        ItemKind::Alias => CompletionItemKind::VARIABLE,
        ItemKind::Column => CompletionItemKind::FIELD,
        ItemKind::Function => CompletionItemKind::FUNCTION,
        ItemKind::Keyword => CompletionItemKind::KEYWORD,
        ItemKind::Type => CompletionItemKind::TYPE_PARAMETER,
    }
}

/// The part of the menu's query that is the word being completed. The menu
/// keeps everything typed since it opened, so after `o.na` or `"Ord` its
/// query still starts at the `o` or the quote.
fn typed_word(query: &str) -> &str {
    let start = query
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .map_or(0, |ix| {
            ix + query[ix..].chars().next().map_or(1, char::len_utf8)
        });
    &query[start..]
}

#[cfg(test)]
mod tests {
    use super::typed_word;

    #[test]
    fn highlights_only_the_word_being_typed() {
        assert_eq!(typed_word("ord"), "ord");
        assert_eq!(typed_word("o.na"), "na");
        assert_eq!(typed_word("public.or"), "or");
        assert_eq!(typed_word("\"Ord"), "Ord");
        assert_eq!(typed_word("o."), "");
        assert_eq!(typed_word("주문.내"), "내");
    }
}
