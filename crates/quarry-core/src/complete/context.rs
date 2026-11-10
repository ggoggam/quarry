//! What the caret's position wants, and which relations it can see.
//!
//! One forward pass over the statement's tokens tracks parentheses (each
//! pair is a scope: a subquery, a function's arguments, a column list) and
//! reads every relation reference: the qualified name after `FROM`, `JOIN`,
//! `INTO`, `UPDATE`, `TABLE` or a comma in a `FROM` list, and the alias
//! after it, with or without `AS`. Tokens after the caret count too, since
//! a select list is usually written before its `FROM`.
//!
//! The position is then read from the tokens just before the caret: whether
//! a relation is expected there, a type, a new name, or an expression, and
//! for an expression which clause it is in.

use std::collections::{HashMap, HashSet};

use super::scan::Scan;
use crate::lex::TokenKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Position {
    /// The statement's first word.
    Start,
    /// Where a relation is named: after `FROM`, `JOIN`, `INTO`, `UPDATE`,
    /// `TABLE`, or a comma in a `FROM` list.
    Relation,
    /// Right after a relation (and its alias) in `FROM` or `JOIN`, where a
    /// keyword such as `WHERE` or `JOIN` comes next.
    AfterRelation,
    /// An expression: the select list, `WHERE`, `ON`, `GROUP BY`, `SET`,
    /// a function's arguments.
    Expression,
    /// `INSERT INTO t (`: the columns of [`Analysis::scope`]`[ix]`.
    ColumnList(usize),
    /// After `::`, `CAST(x AS`, or a column's name in `CREATE TABLE`.
    Type,
    /// A name being made up: after `AS`, `CREATE TABLE`, or where a column
    /// is declared. Nothing fits.
    NewName,
    /// Somewhere only keywords fit, such as after `CREATE` or a column's
    /// type.
    Keyword,
}

/// A relation the statement names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableRef {
    /// The name as written, outermost part first, unquoted.
    pub parts: Vec<String>,
    pub alias: Option<String>,
}

impl TableRef {
    /// What the statement calls it: its alias, or its own name.
    pub fn handle(&self) -> &str {
        self.alias
            .as_deref()
            .unwrap_or_else(|| self.parts.last().map_or("", String::as_str))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Analysis {
    pub position: Position,
    /// The relations the caret can see: those in its own parentheses first,
    /// then those of the queries around it, each in statement order.
    pub scope: Vec<TableRef>,
    /// Names the statement's `WITH` clause defines.
    pub ctes: Vec<String>,
}

/// Words after which a relation is named.
const NAMES_RELATION: &[&str] = &[
    "from", "join", "into", "update", "table", "view", "describe", "using",
];

/// Words that end a relation reference rather than alias it.
const NOT_ALIASES: &[&str] = &[
    "all",
    "and",
    "anti",
    "any",
    "array",
    "as",
    "asof",
    "by",
    "cross",
    "default",
    "delete",
    "do",
    "else",
    "end",
    "except",
    "fetch",
    "final",
    "for",
    "force",
    "from",
    "full",
    "global",
    "group",
    "having",
    "if",
    "ignore",
    "inner",
    "insert",
    "intersect",
    "into",
    "join",
    "lateral",
    "left",
    "limit",
    "natural",
    "not",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "partition",
    "pivot",
    "positional",
    "prewhere",
    "qualify",
    "returning",
    "right",
    "sample",
    "select",
    "semi",
    "set",
    "straight_join",
    "tablesample",
    "then",
    "union",
    "unpivot",
    "update",
    "use",
    "using",
    "values",
    "when",
    "where",
    "window",
    "with",
];

/// Words that start a clause, for telling which one an expression is in.
const CLAUSES: &[&str] = &[
    "select",
    "from",
    "join",
    "into",
    "update",
    "table",
    "where",
    "on",
    "having",
    "by",
    "set",
    "values",
    "using",
    "returning",
    "qualify",
    "window",
    "limit",
    "offset",
    "prewhere",
    "describe",
];

/// Functions whose arguments use `FROM` as a word, not a clause.
const FROM_IN_ARGUMENTS: &[&str] = &["extract", "substring", "trim", "position", "overlay"];

const CASTS: &[&str] = &["cast", "try_cast", "safe_cast"];

pub(super) fn analyze(scan: &Scan) -> Analysis {
    let groups = Groups::of(scan);
    let mut pass = Pass {
        scan,
        groups: &groups,
        refs: Vec::new(),
        expected: HashSet::new(),
        after_ref: HashSet::new(),
        new_names: HashSet::new(),
        column_lists: HashMap::new(),
        create_table_group: None,
    };
    pass.run();
    let ctes = ctes(scan, &groups);

    // The refs the caret can see, nearest scope first.
    let chain = groups.chain(groups.at_caret);
    let mut visible: Vec<(usize, usize)> = pass
        .refs
        .iter()
        .enumerate()
        .filter_map(|(ix, (_, group))| {
            chain
                .iter()
                .position(|g| g == group)
                .map(|depth| (depth, ix))
        })
        .collect();
    visible.sort();
    let position = pass.position(&ctes);
    let position = match position {
        // Point at the ref within `scope`.
        Position::ColumnList(ix) => visible
            .iter()
            .position(|&(_, v)| v == ix)
            .map_or(Position::Expression, Position::ColumnList),
        other => other,
    };
    let scope = visible
        .into_iter()
        .map(|(_, ix)| pass.refs[ix].0.clone())
        .collect();
    Analysis {
        position,
        scope,
        ctes,
    }
}

/// The parentheses of a statement: which pair each token is in.
struct Groups {
    /// For each token, the group it is in. `(` and `)` are in the group
    /// around the pair they make.
    of: Vec<usize>,
    /// For each group, the group around it (the root's is itself) and the
    /// index of its `(`.
    parent: Vec<usize>,
    open: Vec<Option<usize>>,
    /// The group the caret is in.
    at_caret: usize,
}

impl Groups {
    fn of(scan: &Scan) -> Self {
        let mut this = Groups {
            of: Vec::with_capacity(scan.tokens.len()),
            parent: vec![0],
            open: vec![None],
            at_caret: 0,
        };
        let mut stack = vec![0];
        for ix in 0..scan.tokens.len() {
            if ix == scan.at {
                this.at_caret = *stack.last().expect("the root stays");
            }
            let current = *stack.last().expect("the root stays");
            this.of.push(current);
            if scan.is_symbol(ix, "(") {
                let group = this.parent.len();
                this.parent.push(current);
                this.open.push(Some(ix));
                stack.push(group);
            } else if scan.is_symbol(ix, ")") && stack.len() > 1 {
                stack.pop();
                // `)` belongs outside the pair it closes.
                *this.of.last_mut().expect("just pushed") = *stack.last().expect("the root stays");
            }
        }
        if scan.at >= scan.tokens.len() {
            this.at_caret = *stack.last().expect("the root stays");
        }
        this
    }

    /// `group` and the groups around it, innermost first.
    fn chain(&self, mut group: usize) -> Vec<usize> {
        let mut chain = vec![group];
        while group != 0 {
            group = self.parent[group];
            chain.push(group);
        }
        chain
    }

    /// The word just before `group`'s `(`, lower case: the function a pair
    /// of parentheses calls.
    fn caller(&self, scan: &Scan, group: usize) -> Option<String> {
        let open = self.open.get(group).copied().flatten()?;
        let before = open.checked_sub(1)?;
        (scan.tokens[before].kind == TokenKind::Word)
            .then(|| scan.text(before).to_ascii_lowercase())
    }

    /// The group a `(` at token `ix` opens.
    fn opened_at(&self, ix: usize) -> Option<usize> {
        self.open.iter().position(|&open| open == Some(ix))
    }
}

struct Pass<'a, 'b> {
    scan: &'a Scan<'b>,
    groups: &'a Groups,
    /// Every relation reference, with the group it is in.
    refs: Vec<(TableRef, usize)>,
    /// Token positions where a relation's name is expected.
    expected: HashSet<usize>,
    /// Token positions right after a relation reference.
    after_ref: HashSet<usize>,
    /// Token positions where something new is named.
    new_names: HashSet<usize>,
    /// Groups that are an `INSERT` column list, with the ref they list.
    column_lists: HashMap<usize, usize>,
    /// The group of a `CREATE TABLE`'s column definitions.
    create_table_group: Option<usize>,
}

impl Pass<'_, '_> {
    fn run(&mut self) {
        let scan = self.scan;
        let creates = scan.is(0, "create");
        let mut ix = 0;
        while ix < scan.tokens.len() {
            let Some(keyword) = self.relation_keyword(ix) else {
                ix += 1;
                continue;
            };
            if creates && (keyword == "table" || keyword == "view") {
                ix = self.created(ix + 1);
            } else {
                let lists = matches!(keyword.as_str(), "from" | "update" | "using");
                ix = self.references(ix + 1, keyword == "into", lists);
            }
        }
    }

    /// The word at `ix` if a relation is named after it there.
    fn relation_keyword(&self, ix: usize) -> Option<String> {
        let scan = self.scan;
        if scan.tokens[ix].kind != TokenKind::Word {
            return None;
        }
        let word = scan.text(ix).to_ascii_lowercase();
        if !NAMES_RELATION.contains(&word.as_str()) {
            return None;
        }
        let group = self.groups.of[ix];
        match word.as_str() {
            "from"
                if self
                    .groups
                    .caller(scan, group)
                    .is_some_and(|f| FROM_IN_ARGUMENTS.contains(&f.as_str())) =>
            {
                None
            }
            // `JOIN t USING (a, b)` lists columns; `USING t` names a relation.
            "using" if scan.is_symbol(ix + 1, "(") => None,
            _ => Some(word),
        }
    }

    /// `CREATE TABLE [IF NOT EXISTS] name (...)`: a new name, then column
    /// definitions.
    fn created(&mut self, mut ix: usize) -> usize {
        let scan = self.scan;
        self.new_names.insert(ix);
        if scan.is(ix, "if") {
            ix += 1;
            if scan.is(ix, "not") {
                ix += 1;
            }
            if scan.is(ix, "exists") {
                ix += 1;
            }
            self.new_names.insert(ix);
        }
        let Some((_, end)) = self.name(ix) else {
            return ix;
        };
        if scan.is_symbol(end, "(") {
            self.create_table_group = self.groups.opened_at(end);
        }
        end
    }

    /// Relation references from `ix`: one, or a comma-separated list.
    fn references(&mut self, mut ix: usize, into: bool, lists: bool) -> usize {
        let scan = self.scan;
        loop {
            self.expected.insert(ix);
            while scan.is(ix, "only") || scan.is(ix, "lateral") {
                ix += 1;
                self.expected.insert(ix);
            }
            if scan.is(ix, "if") {
                ix += 1;
                if scan.is(ix, "exists") {
                    ix += 1;
                }
                self.expected.insert(ix);
            }
            let Some((parts, end)) = self.name(ix) else {
                return ix;
            };
            // `FROM read_csv(...)`: a table function, not a relation.
            if !into && scan.is_symbol(end, "(") {
                return end;
            }
            let group = self.groups.of[ix];
            self.after_ref.insert(end);
            let (alias, next) = self.alias(end);
            if alias.is_some() {
                self.after_ref.insert(next);
            }
            let ref_ix = self.refs.len();
            self.refs.push((TableRef { parts, alias }, group));
            if into
                && scan.is_symbol(next, "(")
                && let Some(list) = self.groups.opened_at(next)
            {
                self.column_lists.insert(list, ref_ix);
            }
            if lists && scan.is_symbol(next, ",") {
                ix = next + 1;
                continue;
            }
            return next;
        }
    }

    /// A possibly qualified name at `ix`, and the position after it.
    fn name(&self, mut ix: usize) -> Option<(Vec<String>, usize)> {
        let scan = self.scan;
        let first = self.identifier(ix)?;
        let mut parts = vec![first];
        ix += 1;
        while scan.is_symbol(ix, ".")
            && let Some(part) = self.identifier(ix + 1)
        {
            parts.push(part);
            ix += 2;
        }
        Some((parts, ix))
    }

    /// The name at `ix`, unless it is a word that cannot be one here.
    fn identifier(&self, ix: usize) -> Option<String> {
        let scan = self.scan;
        let token = scan.tokens.get(ix)?;
        if token.kind == TokenKind::Word
            && NOT_ALIASES.contains(&scan.text(ix).to_ascii_lowercase().as_str())
        {
            return None;
        }
        scan.name(ix)
    }

    /// An alias at `ix`, with or without `AS`, and the position after it.
    fn alias(&self, ix: usize) -> (Option<String>, usize) {
        if self.scan.is(ix, "as") {
            return match self.identifier(ix + 1) {
                Some(alias) => (Some(alias), ix + 2),
                None => (None, ix + 1),
            };
        }
        match self.identifier(ix) {
            Some(alias) => (Some(alias), ix + 1),
            None => (None, ix),
        }
    }

    fn position(&self, ctes: &[String]) -> Position {
        let scan = self.scan;
        let at = scan.at;
        let group = self.groups.at_caret;
        if self.new_names.contains(&at) {
            return Position::NewName;
        }
        if self.expected.contains(&at) {
            return Position::Relation;
        }
        let Some(prev) = at.checked_sub(1) else {
            return Position::Start;
        };
        if scan.is_symbol(prev, "::") {
            return Position::Type;
        }
        if scan.is(prev, "as") {
            let casts = self
                .groups
                .caller(scan, group)
                .is_some_and(|f| CASTS.contains(&f.as_str()));
            // `WITH name AS (`: a query follows.
            let cte = prev
                .checked_sub(1)
                .and_then(|ix| scan.name(ix))
                .is_some_and(|name| ctes.contains(&name));
            return if casts {
                Position::Type
            } else if cte {
                Position::Keyword
            } else {
                Position::NewName
            };
        }
        if self.create_table_group == Some(group) {
            let starts_definition = |ix: usize| scan.is_symbol(ix, "(") || scan.is_symbol(ix, ",");
            return if starts_definition(prev) {
                Position::NewName
            } else if scan.name(prev).is_some()
                && prev.checked_sub(1).is_some_and(starts_definition)
            {
                Position::Type
            } else {
                Position::Keyword
            };
        }
        if self.after_ref.contains(&at) {
            return Position::AfterRelation;
        }
        if let Some(&ix) = self.column_lists.get(&group) {
            return Position::ColumnList(ix);
        }
        // The nearest clause keyword in the caret's own parentheses.
        let clause = (0..at).rev().find_map(|ix| {
            (self.groups.of[ix] == group && scan.tokens[ix].kind == TokenKind::Word)
                .then(|| scan.text(ix).to_ascii_lowercase())
                .filter(|word| CLAUSES.contains(&word.as_str()))
        });
        match clause.as_deref() {
            Some("from" | "join" | "into" | "update" | "table" | "describe") => {
                Position::AfterRelation
            }
            Some(_) => Position::Expression,
            None if group != 0 => Position::Expression,
            None => Position::Keyword,
        }
    }
}

/// The names `WITH a AS (...), b AS (...)` defines at the statement's start.
fn ctes(scan: &Scan, groups: &Groups) -> Vec<String> {
    let mut names = Vec::new();
    if !scan.is(0, "with") {
        return names;
    }
    let mut ix = 1;
    if scan.is(ix, "recursive") {
        ix += 1;
    }
    let close = |open: usize| {
        let group = groups.opened_at(open)?;
        let level = groups.parent[group];
        (open + 1..scan.tokens.len()).find(|&ix| groups.of[ix] == level && scan.is_symbol(ix, ")"))
    };
    loop {
        let Some(name) = scan.name(ix) else {
            return names;
        };
        if scan.tokens[ix].kind == TokenKind::Word
            && NOT_ALIASES.contains(&name.to_ascii_lowercase().as_str())
        {
            return names;
        }
        names.push(name);
        ix += 1;
        // `name (a, b) AS`: the column names.
        if scan.is_symbol(ix, "(") {
            let Some(end) = close(ix) else {
                return names;
            };
            ix = end + 1;
        }
        if !scan.is(ix, "as") {
            return names;
        }
        ix += 1;
        if scan.is(ix, "not") {
            ix += 1;
        }
        if scan.is(ix, "materialized") {
            ix += 1;
        }
        if !scan.is_symbol(ix, "(") {
            return names;
        }
        let Some(end) = close(ix) else {
            return names;
        };
        ix = end + 1;
        if !scan.is_symbol(ix, ",") {
            return names;
        }
        ix += 1;
    }
}
