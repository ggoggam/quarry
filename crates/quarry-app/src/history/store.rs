//! The query history as plain data, and `history.json` in Quarry's settings
//! folder where it is kept.
//!
//! Every statement Quarry sends is one [`Entry`]: its text, when it ran, on
//! which connection, how long it took, how many rows, and how it ended. The
//! connection is its password-free key ([`crate::editor::connection_key`]),
//! never a URL with a password.
//!
//! The [`Log`] keeps the newest [`MAX_ENTRIES`] entries within
//! [`MAX_TOTAL_BYTES`] of text, and cuts a statement longer than
//! [`MAX_STATEMENT_BYTES`]. The file is written whole, so the caller writes it
//! off the UI thread and not after every statement.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

/// The most entries kept; older ones are dropped.
pub const MAX_ENTRIES: usize = 2_000;
/// The most text the entries hold together, statements and messages.
pub const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
/// A longer statement is kept cut to this many bytes, and marked as cut.
pub const MAX_STATEMENT_BYTES: usize = 32 * 1024;
/// A longer error message is kept cut to this many bytes.
pub const MAX_ERROR_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub statement: String,
    /// The statement was longer than [`MAX_STATEMENT_BYTES`] and is cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// The connection's password-free key.
    pub connection: String,
    /// The connection's dialect, as `Dialect::id` names it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dialect: String,
    /// When it started, in milliseconds since the Unix epoch.
    pub at: i64,
    pub elapsed_ms: u64,
    pub outcome: Outcome,
    /// Rows it returned, for a statement that returns rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Rows it changed, when the engine said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<u64>,
    /// The engine's message, for a statement that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Entry {
    /// Bytes it counts against [`MAX_TOTAL_BYTES`].
    fn size(&self) -> usize {
        self.statement.len()
            + self.connection.len()
            + self.error.as_ref().map_or(0, String::len)
            + 64
    }

    /// Cut what is too long to keep.
    fn clamp(&mut self) {
        if self.statement.len() > MAX_STATEMENT_BYTES {
            self.statement
                .truncate(floor_char_boundary(&self.statement, MAX_STATEMENT_BYTES));
            self.cut = true;
        }
        if let Some(error) = &mut self.error
            && error.len() > MAX_ERROR_BYTES
        {
            error.truncate(floor_char_boundary(error, MAX_ERROR_BYTES));
            error.push('…');
        }
    }

    /// What a search looks in: the statement and the connection, in lower
    /// case. Worked out once per entry, not once per keystroke.
    pub fn haystack(&self) -> String {
        format!("{}\n{}", self.statement, self.connection).to_lowercase()
    }
}

/// A search box's text as the terms [`matches`] takes.
pub fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether every term is in an entry's [`Entry::haystack`].
pub fn matches(haystack: &str, terms: &[String]) -> bool {
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

fn floor_char_boundary(text: &str, at: usize) -> usize {
    (0..=at.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

/// The entries, oldest first, within the caps.
#[derive(Debug, Default, Clone)]
pub struct Log {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl Log {
    /// Add the newest entry. Returns whether older entries were dropped.
    pub fn push(&mut self, mut entry: Entry) -> bool {
        entry.clamp();
        self.bytes += entry.size();
        self.entries.push_back(entry);
        self.trim()
    }

    /// Put entries from before this run in front of the ones recorded since
    /// Quarry started. Returns whether some did not fit.
    pub fn prepend(&mut self, older: Vec<Entry>) -> bool {
        for mut entry in older.into_iter().rev() {
            entry.clamp();
            self.bytes += entry.size();
            self.entries.push_front(entry);
        }
        self.trim()
    }

    fn trim(&mut self) -> bool {
        let mut trimmed = false;
        while self.entries.len() > MAX_ENTRIES
            || (self.bytes > MAX_TOTAL_BYTES && self.entries.len() > 1)
        {
            let Some(oldest) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= oldest.size();
            trimmed = true;
        }
        trimmed
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Newest first, as the history shows them.
    pub fn newest_first(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev()
    }

    /// Oldest first, as the file keeps them.
    pub fn to_vec(&self) -> Vec<Entry> {
        self.entries.iter().cloned().collect()
    }
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    /// Entries a later Quarry wrote that this one cannot read are dropped,
    /// not the whole file.
    #[serde(default, deserialize_with = "lenient")]
    entries: Vec<Entry>,
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Entry>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(entry) => Some(entry),
            Err(err) => {
                log::warn!("skipping a history entry Quarry cannot read: {err}");
                None
            }
        })
        .collect())
}

/// The entries in the history file, oldest first; none when there is no
/// file. A file that is not a history is moved aside to `history.json.bak`.
pub fn load(path: &Path) -> anyhow::Result<Vec<Entry>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_slice::<HistoryFile>(&bytes) {
        Ok(file) => Ok(file.entries),
        Err(err) => {
            let backup = path.with_extension("json.bak");
            log::warn!(
                "{} is not a query history ({err}); moving it to {}",
                path.display(),
                backup.display()
            );
            let _ = std::fs::rename(path, &backup);
            Ok(Vec::new())
        }
    }
}

/// Write the history file whole, replacing it in one step.
pub fn save(path: &Path, entries: Vec<Entry>) -> anyhow::Result<()> {
    let file = HistoryFile {
        version: VERSION,
        entries,
    };
    let bytes = serde_json::to_vec(&file)?;
    write_atomically(path, &bytes).with_context(|| format!("saving {}", path.display()))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(statement: &str, at: i64) -> Entry {
        Entry {
            statement: statement.into(),
            cut: false,
            connection: ":memory:".into(),
            dialect: "duckdb".into(),
            at,
            elapsed_ms: 3,
            outcome: Outcome::Succeeded,
            rows: Some(1),
            affected: None,
            error: None,
        }
    }

    #[test]
    fn keeps_the_newest_entries() {
        let mut log = Log::default();
        for i in 0..MAX_ENTRIES + 5 {
            log.push(entry(&format!("select {i}"), i as i64));
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        let newest: Vec<_> = log.newest_first().take(2).map(|e| e.at).collect();
        assert_eq!(newest, [MAX_ENTRIES as i64 + 4, MAX_ENTRIES as i64 + 3]);
        assert_eq!(log.to_vec()[0].at, 5, "the oldest five went");
    }

    #[test]
    fn keeps_within_the_byte_budget_and_cuts_long_statements() {
        let mut log = Log::default();
        let long = "x".repeat(MAX_STATEMENT_BYTES * 2);
        log.push(entry(&long, 0));
        let kept = &log.to_vec()[0];
        assert!(kept.cut);
        assert_eq!(kept.statement.len(), MAX_STATEMENT_BYTES);

        let fit = MAX_TOTAL_BYTES / (MAX_STATEMENT_BYTES + 64 + ":memory:".len());
        for i in 0..fit + 10 {
            log.push(entry(&long, i as i64));
        }
        assert!(log.len() <= fit, "{} entries of {fit}", log.len());
        assert!(log.bytes <= MAX_TOTAL_BYTES);

        // A cut never splits a character.
        let mut wide = entry(&"é".repeat(MAX_STATEMENT_BYTES), 0);
        wide.error = Some("ü".repeat(MAX_ERROR_BYTES));
        wide.clamp();
        assert!(wide.statement.len() <= MAX_STATEMENT_BYTES);
        assert!(wide.error.unwrap().ends_with('…'));
    }

    #[test]
    fn prepends_what_the_file_had() {
        let mut log = Log::default();
        log.push(entry("select 'new'", 10));
        log.prepend(vec![entry("select 'old'", 1), entry("select 'older'", 2)]);
        let order: Vec<_> = log.newest_first().map(|e| e.at).collect();
        assert_eq!(order, [10, 2, 1]);
    }

    #[test]
    fn searches_statements_and_connections() {
        let mut orders = entry("SELECT * FROM Orders", 0);
        orders.connection = "postgres://alice@db/shop".into();
        let terms = search_terms("  orders  SHOP ");
        assert_eq!(terms, ["orders", "shop"]);
        let haystack = orders.haystack();
        assert!(matches(&haystack, &terms));
        assert!(matches(&haystack, &[]));
        assert!(!matches(&haystack, &search_terms("orders mysql")));
    }

    #[test]
    fn round_trips_and_reads_tolerantly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Quarry/history.json");
        assert_eq!(load(&path).unwrap(), [], "no file, no history");

        let mut failed = entry("SELECT nope", 5);
        failed.outcome = Outcome::Failed;
        failed.rows = None;
        failed.error = Some("Binder Error: column nope not found".into());
        save(&path, vec![entry("SELECT 1", 4), failed.clone()]).unwrap();
        assert_eq!(load(&path).unwrap(), [entry("SELECT 1", 4), failed]);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.starts_with(r#"{"version":1,"#), "{json}");
        assert!(json.contains(r#""outcome":"failed""#), "{json}");
        assert!(!json.contains("null") && !json.contains("cut"), "{json}");

        // A later Quarry's entry that this one cannot read is skipped.
        std::fs::write(
            &path,
            r#"{"version": 2, "entries": [
                {"statement": "SELECT 1", "connection": "x", "at": 1, "elapsed_ms": 2,
                 "outcome": "succeeded", "tags": ["new"]},
                {"statement": "SELECT 2", "connection": "x", "at": 2, "elapsed_ms": 2,
                 "outcome": "timed_out"}
            ]}"#,
        )
        .unwrap();
        let entries = load(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].statement, "SELECT 1");

        // A broken file is moved aside, not lost.
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path).unwrap(), []);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Quarry/history.json.bak")).unwrap(),
            "{ not json"
        );
    }
}
