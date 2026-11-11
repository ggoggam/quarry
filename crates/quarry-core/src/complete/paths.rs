//! Completing file paths in string literals, where the engine reads a file:
//! DuckDB's `FROM 'orders.csv'`, `read_parquet('…')`, `ATTACH '…'` and
//! `COPY … TO '…'`, and SQLite's `ATTACH '…'`.
//!
//! The folder typed so far is listed as the caret moves through it: `data/`
//! lists `data`, `~/` the home folder, and a relative path the folder the
//! engine resolves relative paths against, which the caller passes in.

use std::path::{Path, PathBuf};

use super::{Completions, Item, ItemKind, LIMIT, Trigger};
use crate::Dialect;
use crate::lex::{Token, TokenKind, tokenize};

/// Table functions whose first argument is a file path, or a list of them.
const PATH_FUNCTIONS: &[&str] = &[
    "read_csv",
    "read_csv_auto",
    "sniff_csv",
    "read_json",
    "read_json_auto",
    "read_json_objects",
    "read_json_objects_auto",
    "read_ndjson",
    "read_ndjson_auto",
    "read_ndjson_objects",
    "read_parquet",
    "parquet_scan",
    "parquet_metadata",
    "parquet_schema",
    "parquet_file_metadata",
    "parquet_kv_metadata",
    "read_xlsx",
    "read_avro",
    "read_text",
    "read_blob",
    "glob",
    "st_read",
    "delta_scan",
    "iceberg_scan",
];

/// Extensions of files a `FROM` or a reader can read, after any `.gz` or
/// `.zst`.
const DATA_EXTENSIONS: &[&str] = &[
    "csv", "tsv", "tab", "psv", "txt", "json", "jsonl", "ndjson", "parquet", "pq", "arrow",
    "arrows", "feather", "ipc", "xlsx", "avro",
];

/// What the string is a path to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wants {
    /// A file to read rows from: folders and data files are offered.
    Data,
    /// Any file, such as a database to attach or a file to write.
    Any,
}

/// Path completions for a caret at byte `caret` in `sql`, or `None` when the
/// caret is not in a string that names a file (then [`super::complete`]
/// applies). `base` is the folder relative paths are read from.
pub fn complete_path(
    sql: &str,
    caret: usize,
    dialect: Dialect,
    base: &Path,
    trigger: Trigger,
) -> Option<Completions> {
    if !matches!(dialect, Dialect::DuckDb | Dialect::Sqlite) {
        return None;
    }
    let caret = caret.min(sql.len());
    if !sql.is_char_boundary(caret) {
        return None;
    }
    let tokens = tokenize(sql, &dialect.lexical());
    let at = tokens.iter().position(|t| {
        t.kind == TokenKind::String
            && t.range.start < caret
            && (caret < t.range.end || !closed(t.text(sql)))
    })?;
    let string = &tokens[at];
    // Plain '…' strings only: not E'…', X'…' or $$…$$.
    if !string.text(sql).starts_with('\'') {
        return None;
    }
    let wants = wants(sql, &tokens[..at], dialect)?;

    let open = string.range.start + 1;
    let typed = &sql[open..caret];
    // The part after the last `/` is the name being typed; the rest names
    // the folder to list.
    let name_start = typed.rfind('/').map_or(open, |ix| open + ix + 1);
    let folder = unescape(&sql[open..name_start]);
    let prefix = unescape(&sql[name_start..caret]);
    let content_end = if closed(string.text(sql)) {
        string.range.end - 1
    } else {
        string.range.end
    };
    let name_end = sql[caret..content_end]
        .find('/')
        .map_or(content_end, |ix| caret + ix);
    let replace = name_start..name_end;

    let typed_name = unescape(&sql[name_start..name_end]);
    let mut items = list(&resolve(&folder, base), &prefix, wants);
    if trigger == Trigger::Typing {
        items.retain(|item| item.label != typed_name);
    }
    Some(Completions { replace, items })
}

/// What a string just after `before` would name, if it is a path.
fn wants(sql: &str, before: &[Token], dialect: Dialect) -> Option<Wants> {
    let significant: Vec<&Token> = before.iter().filter(|t| !t.kind.is_trivia()).collect();
    // Back to the token the string (or the list it is in) follows.
    let mut ix = significant.len();
    let previous = |ix: usize| ix.checked_sub(1).map(|ix| significant[ix]);
    let word = |token: Option<&&Token>| {
        token
            .filter(|t| t.kind == TokenKind::Word)
            .map(|t| t.text(sql).to_ascii_lowercase())
    };
    let symbol = |token: Option<&&Token>, symbol: &str| {
        token.is_some_and(|t| t.kind != TokenKind::String && t.text(sql) == symbol)
    };
    // `['a.csv', 'b.csv', '…`: skip the strings before it in the list.
    while symbol(previous(ix).as_ref(), ",")
        && previous(ix - 1).is_some_and(|t| t.kind == TokenKind::String)
    {
        ix -= 2;
    }
    if symbol(previous(ix).as_ref(), "[") {
        ix -= 1;
    }
    if symbol(previous(ix).as_ref(), "(") {
        let function = word(previous(ix - 1).as_ref())?;
        return PATH_FUNCTIONS
            .contains(&function.as_str())
            .then_some(Wants::Data)
            .filter(|_| dialect == Dialect::DuckDb);
    }
    let keyword = word(previous(ix).as_ref())?;
    match (dialect, keyword.as_str()) {
        (Dialect::DuckDb, "from" | "join") => {
            // `COPY t FROM 'file'` reads a file too.
            Some(Wants::Data)
        }
        (_, "attach") => Some(Wants::Any),
        // SQLite's `ATTACH DATABASE 'file'`, DuckDB's `ATTACH DATABASE` and
        // `EXPORT DATABASE 'folder'` / `IMPORT DATABASE 'folder'`.
        (_, "database") => Some(Wants::Any),
        (Dialect::DuckDb, "to") => {
            // `COPY … TO 'file'`, not `GRANT … TO` or `ALTER … RENAME TO`.
            let copy = significant[..ix]
                .iter()
                .any(|t| t.kind == TokenKind::Word && t.text(sql).eq_ignore_ascii_case("copy"));
            copy.then_some(Wants::Any)
        }
        _ => None,
    }
}

/// The folder `typed` names: under the home folder for `~/`, as is when
/// absolute, else under `base`.
fn resolve(typed: &str, base: &Path) -> PathBuf {
    if let Some(rest) = typed.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    if typed.is_empty() {
        return base.to_owned();
    }
    let path = Path::new(typed);
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

/// The entries of `folder` that start with `prefix` (ignoring case), then
/// those that contain it: folders first, then files, each by name. Hidden
/// ones only when the prefix starts with a dot.
fn list(folder: &Path, prefix: &str, wants: Wants) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let lower = prefix.to_lowercase();
    let mut found: Vec<(u8, bool, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        // Follows symlinks, so a linked folder lists as one.
        let is_dir = entry.path().is_dir();
        if !is_dir && wants == Wants::Data && !is_data_file(&name) {
            continue;
        }
        let name_lower = name.to_lowercase();
        let rank = if name_lower.starts_with(&lower) {
            0
        } else if !lower.is_empty() && name_lower.contains(&lower) {
            1
        } else {
            continue;
        };
        found.push((rank, !is_dir, name));
    }
    found.sort_by_key(|(rank, is_file, name)| (*rank, *is_file, name.to_lowercase()));
    found.truncate(LIMIT);
    found
        .into_iter()
        .map(|(_, is_file, name)| {
            let (label, kind, detail) = if is_file {
                (name, ItemKind::File, "file")
            } else {
                (format!("{name}/"), ItemKind::Folder, "folder")
            };
            Item {
                insert: label.replace('\'', "''"),
                label,
                kind,
                detail: detail.to_owned(),
            }
        })
        .collect()
}

fn is_data_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let lower = ["gz", "zst", "zstd"]
        .iter()
        .find_map(|c| lower.strip_suffix(&format!(".{c}")))
        .unwrap_or(&lower);
    lower
        .rsplit_once('.')
        .is_some_and(|(_, extension)| DATA_EXTENSIONS.contains(&extension))
}

/// A string literal's text with its quotes' doubling undone.
fn unescape(text: &str) -> String {
    text.replace("''", "'")
}

/// Whether a `'…'` literal is closed: it ends with an odd run of quotes.
fn closed(text: &str) -> bool {
    let body = &text[1..];
    let run = body.bytes().rev().take_while(|&b| b == b'\'').count();
    !body.is_empty() && run % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Completions at the `|` in `sql`, against `base`.
    fn at(sql: &str, base: &Path) -> Option<(String, Vec<String>)> {
        let caret = sql.find('|').unwrap();
        let sql = sql.replacen('|', "", 1);
        let completions = complete_path(&sql, caret, Dialect::DuckDb, base, Trigger::Invoked)?;
        let replaced = sql[completions.replace.clone()].to_owned();
        Some((
            replaced,
            completions.items.into_iter().map(|i| i.insert).collect(),
        ))
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "orders.csv",
            "Orders 2024.parquet",
            "events.json.gz",
            "notes.md",
            "shop.duckdb",
            ".hidden.csv",
            "it's.csv",
            "data/users.csv",
            "data/teams.feather",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        std::fs::create_dir(dir.path().join("archive")).unwrap();
        dir
    }

    #[test]
    fn lists_folders_then_data_files_after_from() {
        let dir = tree();
        let base = dir.path();
        assert_eq!(
            at("SELECT * FROM '|", base).unwrap().1,
            [
                "archive/",
                "data/",
                "events.json.gz",
                "it''s.csv",
                "Orders 2024.parquet",
                "orders.csv"
            ]
        );
        assert_eq!(
            at("SELECT * FROM 'ord|'", base).unwrap(),
            (
                "ord".into(),
                vec!["Orders 2024.parquet".into(), "orders.csv".into()]
            )
        );
        // Within a word: the rest of it is replaced too.
        assert_eq!(at("FROM 'or|ders.csv' o", base).unwrap().0, "orders.csv");
        // Inside a folder.
        assert_eq!(
            at("SELECT * FROM orders o JOIN 'data/t|' t USING (id)", base).unwrap(),
            ("t".into(), vec!["teams.feather".into()])
        );
        // Hidden files when asked for.
        assert_eq!(at("FROM '.h|", base).unwrap().1, [".hidden.csv"]);
        // Absolute paths.
        let absolute = format!("FROM '{}/data/|", base.display());
        assert_eq!(
            at(&absolute, base).unwrap().1,
            ["teams.feather", "users.csv"]
        );
        // A match inside the name ranks after the prefix ones.
        assert_eq!(at("FROM '2024|", base).unwrap().1, ["Orders 2024.parquet"]);
    }

    #[test]
    fn completes_reader_arguments_and_lists() {
        let dir = tree();
        let base = dir.path();
        assert_eq!(
            at("FROM read_csv('ord|')", base).unwrap().1,
            ["Orders 2024.parquet", "orders.csv"]
        );
        assert_eq!(
            at("FROM read_parquet(['orders.csv', 'data/|'])", base)
                .unwrap()
                .1,
            ["teams.feather", "users.csv"]
        );
        // Not the reader's other arguments.
        assert_eq!(at("FROM read_csv('orders.csv', delim = '|')", base), None);
        assert_eq!(at("SELECT upper('|')", base), None);
    }

    #[test]
    fn offers_any_file_where_one_is_written_or_attached() {
        let dir = tree();
        let base = dir.path();
        assert!(
            at("ATTACH 'sh|", base)
                .unwrap()
                .1
                .contains(&"shop.duckdb".to_owned())
        );
        assert!(
            at("COPY orders TO 'no|", base)
                .unwrap()
                .1
                .contains(&"notes.md".to_owned())
        );
        assert!(
            !at("FROM 'no|", base)
                .unwrap()
                .1
                .contains(&"notes.md".to_owned())
        );
        assert_eq!(at("SELECT 'from |'", base), None);
        assert_eq!(at("SELECT * FROM t WHERE name = '|'", base), None);
        // Other engines read files on the server, if at all.
        let sql = "SELECT * FROM '";
        assert!(complete_path(sql, sql.len(), Dialect::Postgres, base, Trigger::Invoked).is_none());
        let sql = "ATTACH DATABASE '";
        assert!(complete_path(sql, sql.len(), Dialect::Sqlite, base, Trigger::Invoked).is_some());
    }

    #[test]
    fn leaves_out_what_is_typed_already() {
        let dir = tree();
        let sql = "FROM 'orders.csv";
        let typing =
            complete_path(sql, sql.len(), Dialect::DuckDb, dir.path(), Trigger::Typing).unwrap();
        assert!(typing.items.is_empty());
        // A folder that is not there lists nothing.
        let sql = "FROM 'missing/";
        let none = complete_path(
            sql,
            sql.len(),
            Dialect::DuckDb,
            dir.path(),
            Trigger::Invoked,
        )
        .unwrap();
        assert!(none.items.is_empty());
    }
}
