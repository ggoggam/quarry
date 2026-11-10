//! The dialect's keywords, functions and types, as completion offers them.

use std::sync::OnceLock;

use crate::dialect::{Dialect, Flavor};
use crate::vocabulary::{Availability, Entry, Vocabulary, WordClass};

/// One word of a dialect's vocabulary.
#[derive(Debug, Clone, Copy)]
pub(super) struct VocabWord {
    /// Lower case, as matched.
    pub lower: &'static str,
    pub entry: &'static Entry,
}

impl VocabWord {
    pub fn available(&self, flavor: &Flavor) -> bool {
        self.entry.availability(flavor) == Availability::Available
    }
}

/// `dialect`'s words, sorted, built once.
pub(super) fn of(dialect: Dialect) -> &'static [VocabWord] {
    static CACHE: [OnceLock<Vec<VocabWord>>; 6] = [const { OnceLock::new() }; 6];
    let ix = Dialect::ALL
        .iter()
        .position(|d| *d == dialect)
        .expect("every dialect is in ALL");
    CACHE[ix].get_or_init(|| {
        let vocabulary = Vocabulary::of(dialect);
        let mut words: Vec<VocabWord> = [WordClass::Keyword, WordClass::Function, WordClass::Type]
            .into_iter()
            .flat_map(|class| vocabulary.words(class))
            .map(|(lower, entry)| VocabWord { lower, entry })
            .collect();
        words.sort_by_key(|w| (w.lower, w.entry.class as u8));
        words
    })
}

/// Which case to write a word in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Case {
    Upper,
    Lower,
}

impl Case {
    /// The case `typed` is in, if it has letters: upper when it has an
    /// upper case letter, so `S` and `Sel` both mean `SELECT`.
    pub fn of(typed: &str) -> Option<Case> {
        let mut letters = typed.chars().filter(|c| c.is_alphabetic()).peekable();
        letters.peek()?;
        Some(if letters.any(char::is_uppercase) {
            Case::Upper
        } else {
            Case::Lower
        })
    }

    pub fn apply(self, word: &str) -> String {
        match self {
            Case::Upper => word.to_uppercase(),
            Case::Lower => word.to_lowercase(),
        }
    }
}

/// How to write vocabulary word `word` for the user: in `case` when the
/// engine ignores its case. A spelling with capitals (ClickHouse's `toDate`,
/// `UInt64`) is the engine's own and kept as it is.
pub(super) fn spell(word: &VocabWord, case: Option<Case>) -> String {
    let spelling = word.entry.spelling;
    if spelling.chars().any(char::is_uppercase) {
        return spelling.to_owned();
    }
    match case {
        Some(case) => case.apply(spelling),
        None => spelling.to_owned(),
    }
}

/// Keywords that start a statement, first at the start of one.
pub(super) const STATEMENT_STARTS: &[&str] = &[
    "select", "with", "insert", "update", "delete", "create", "drop", "alter", "explain", "show",
    "describe", "set", "begin", "commit", "rollback", "truncate", "copy", "attach", "use",
    "pragma", "values", "table", "grant", "merge", "call", "vacuum", "analyze",
];

/// Keywords that follow a relation in `FROM`, first right after one.
pub(super) const AFTER_RELATION: &[&str] = &[
    "where", "join", "left", "inner", "on", "as", "group", "order", "limit", "right", "full",
    "cross", "natural", "using", "having", "union", "window", "qualify", "offset", "set", "values",
];

/// Keywords common enough in an expression to come before the rest.
pub(super) const IN_EXPRESSIONS: &[&str] = &[
    "from", "and", "or", "not", "as", "is", "null", "in", "like", "between", "case", "when",
    "then", "else", "end", "distinct", "exists", "order", "group", "limit", "where",
];
