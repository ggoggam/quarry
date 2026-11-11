//! Completion in the SQL editor, from the catalog and the dialect's words.
//!
//! There is no parser here, on purpose: the text is being typed, so it is
//! almost never valid SQL. Completion reads the dialect's tokens around the
//! caret instead, in the statement the caret is in, and works out just
//! enough to rank names well:
//!
//! - [`scan`]: the statement's tokens, the word being typed and the names
//!   qualifying it (`schema.`, `alias.`). Nothing is offered in a string or
//!   a comment.
//! - [`context`]: what the position wants (a relation after `FROM`, a column
//!   in `WHERE`, a type after `::`), and the relations the statement names,
//!   with their aliases.
//! - [`index`]: the catalog flattened for matching, built once per catalog
//!   so a keystroke only walks flat lists.
//! - [`rank`]: matching the typed prefix and ordering what matched.
//! - [`words`]: the dialect's keywords, functions and types, and the case to
//!   write them in.
//! - [`paths`]: file paths in the strings DuckDB reads files from, which
//!   [`complete_path`] lists from the file system.
//!
//! [`complete`] puts them together. It is plain and synchronous: a keystroke
//! costs a tokenize of the text (as highlighting already does) and one pass
//! over the candidates the position allows.

mod candidates;
mod context;
mod index;
mod paths;
mod rank;
mod scan;
mod words;

#[cfg(test)]
mod tests;

use std::ops::Range;

pub use index::CatalogIndex;
pub use paths::complete_path;

use crate::Flavor;

/// At most this many items are returned, best first.
pub const LIMIT: usize = 100;

/// What an item is, for the menu to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Database,
    Schema,
    Table,
    View,
    MaterializedView,
    /// A name a `WITH` clause defines.
    Cte,
    /// A table alias, or a table's own name, the statement already uses.
    Alias,
    Column,
    Function,
    Keyword,
    Type,
    File,
    Folder,
}

impl ItemKind {
    /// How the menu names the kind.
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Database => "database",
            ItemKind::Schema => "schema",
            ItemKind::Table => "table",
            ItemKind::View => "view",
            ItemKind::MaterializedView => "materialized view",
            ItemKind::Cte => "cte",
            ItemKind::Alias => "alias",
            ItemKind::Column => "column",
            ItemKind::Function => "function",
            ItemKind::Keyword => "keyword",
            ItemKind::Type => "type",
            ItemKind::File => "file",
            ItemKind::Folder => "folder",
        }
    }
}

/// One suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The name as the menu shows and matches it, unquoted.
    pub label: String,
    /// What replaces [`Completions::replace`]: quoted when the dialect needs
    /// it, qualified when the name alone would not resolve, keywords in the
    /// case being typed.
    pub insert: String,
    pub kind: ItemKind,
    /// What the item is and where it is from: `table · public`,
    /// `integer · orders`.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completions {
    /// The byte range the chosen item replaces: the word at the caret, or an
    /// empty range at the caret.
    pub replace: Range<usize>,
    pub items: Vec<Item>,
}

/// How completion was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Asked for (ctrl-space): even an empty word gets suggestions.
    Invoked,
    /// Typing: only a word, a `.` after a qualifier or an opened quote does,
    /// and an item that would leave the text as it is is left out, so Enter
    /// after a finished word still starts a new line.
    Typing,
}

/// The completions for a caret at byte `caret` in `sql`, written in
/// `flavor`, against `catalog`.
pub fn complete(
    sql: &str,
    caret: usize,
    flavor: &Flavor,
    catalog: &CatalogIndex,
    trigger: Trigger,
) -> Completions {
    let caret = floor_char_boundary(sql, caret.min(sql.len()));
    let Some(scan) = scan::scan(sql, caret, &flavor.dialect.lexical()) else {
        return Completions {
            replace: caret..caret,
            items: Vec::new(),
        };
    };
    let replace = scan.word.range.clone();
    if trigger == Trigger::Typing
        && scan.word.prefix.is_empty()
        && scan.qualifier.is_empty()
        && scan.word.quote.is_none()
    {
        return Completions {
            replace,
            items: Vec::new(),
        };
    }
    let analysis = context::analyze(&scan);
    // After `FROM orders`, a word being typed is as likely an alias as a
    // keyword: wait for two letters, so Enter after `o` is still a newline.
    if trigger == Trigger::Typing
        && analysis.position == context::Position::AfterRelation
        && scan.qualifier.is_empty()
        && scan.word.prefix.chars().count() < 2
    {
        return Completions {
            replace,
            items: Vec::new(),
        };
    }
    let mut items = candidates::collect(&scan, &analysis, flavor, catalog);
    if trigger == Trigger::Typing {
        let typed = &sql[replace.clone()];
        // A finished word: what ranks first is what is typed. Showing the
        // rest would make Enter, meant for a new line, replace it.
        if items
            .first()
            .is_some_and(|best| best.insert.eq_ignore_ascii_case(typed) || best.label == typed)
        {
            items.clear();
        }
        items.retain(|item| item.insert != typed);
    }
    Completions { replace, items }
}

fn floor_char_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}
