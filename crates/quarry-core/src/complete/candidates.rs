//! Gathering and ordering the items for a position.
//!
//! Each position draws on its own sources in its own order of importance
//! (after `FROM`, relations before functions before keywords; in `WHERE`,
//! the statement's columns before functions before keywords). Items sort by
//! how well they matched first and that order second, so a name starting
//! with what was typed beats one that merely contains it, whatever it is.

use std::collections::{HashMap, HashSet};

use super::context::{Analysis, Position, TableRef};
use super::index::CatalogIndex;
use super::rank::{Tier, tier};
use super::scan::Scan;
use super::words::{self, Case, VocabWord};
use super::{Item, ItemKind, LIMIT};
use crate::catalog::RelationKind;
use crate::dialect::{Dialect, Flavor};
use crate::lex::TokenKind;
use crate::vocabulary::{Vocabulary, WordClass};

/// Orders items: by match tier, then the source's place in the position,
/// then promoted keywords in their order, then an exact match, then a
/// prefix in the case typed, then within the source.
type Key = (Tier, u8, u8, bool, bool, u64);

/// How a name matched what is typed.
struct Grade {
    tier: Tier,
    exact: bool,
    /// It starts with what is typed, case and all.
    same_case: bool,
}

struct Collector<'a> {
    /// What is typed, lower case.
    query: String,
    /// What is typed, as typed.
    typed: &'a str,
    dialect: Dialect,
    quote: Option<char>,
    catalog: &'a CatalogIndex,
    found: Vec<(Key, Item)>,
}

pub(super) fn collect(
    scan: &Scan,
    analysis: &Analysis,
    flavor: &Flavor,
    catalog: &CatalogIndex,
) -> Vec<Item> {
    let mut out = Collector {
        query: scan.word.prefix.to_lowercase(),
        typed: &scan.word.prefix,
        dialect: flavor.dialect,
        quote: scan.word.quote,
        catalog,
        found: Vec::new(),
    };
    let typed = Case::of(&scan.word.prefix);
    let keyword_case = typed.or_else(|| earlier_keyword_case(scan, flavor.dialect));
    let words = Words {
        flavor,
        keyword_case: keyword_case.unwrap_or(Case::Upper),
        function_case: typed,
    };

    if !scan.qualifier.is_empty() {
        out.qualified(&scan.qualifier, analysis);
    } else {
        match &analysis.position {
            Position::Start => words.add(&mut out, WordClass::Keyword, 0, words::STATEMENT_STARTS),
            Position::Relation => {
                out.ctes(&analysis.ctes, 0);
                out.relations(1);
                out.schemas(2);
                out.databases(3);
                if !out.query.is_empty() {
                    words.add(&mut out, WordClass::Function, 4, &[]);
                    words.add(&mut out, WordClass::Keyword, 5, &[]);
                }
            }
            Position::AfterRelation => {
                words.add(&mut out, WordClass::Keyword, 0, words::AFTER_RELATION)
            }
            Position::Keyword => words.add(&mut out, WordClass::Keyword, 0, &[]),
            Position::Expression => {
                out.scope_columns(&analysis.scope, &analysis.ctes, 0);
                out.handles(&analysis.scope, &analysis.ctes, 1);
                words.add(&mut out, WordClass::Function, 2, &[]);
                words.add(&mut out, WordClass::Keyword, 3, words::IN_EXPRESSIONS);
            }
            Position::ColumnList(ix) => {
                if let Some(target) = analysis.scope.get(*ix) {
                    let target = std::slice::from_ref(target);
                    out.scope_columns(target, &analysis.ctes, 0);
                }
            }
            Position::Type => words.add(&mut out, WordClass::Type, 0, &[]),
            Position::NewName => {}
        }
    }

    let mut found = out.found;
    if scan.word.quote.is_some() {
        // In a quoted name only names fit.
        found.retain(|(_, item)| {
            !matches!(
                item.kind,
                ItemKind::Keyword | ItemKind::Function | ItemKind::Type
            )
        });
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(&b.1.label)));
    // A word that is both a keyword and a function (`left`, `year`) shows once,
    // as whichever the position ranks higher.
    let mut words_seen = HashSet::new();
    found.retain(|(_, item)| {
        !matches!(
            item.kind,
            ItemKind::Keyword | ItemKind::Function | ItemKind::Type
        ) || words_seen.insert(item.insert.to_lowercase())
    });
    found.truncate(LIMIT);
    found.into_iter().map(|(_, item)| item).collect()
}

/// The case of the last keyword before the caret, so `select * from |`
/// carries on in lower case.
fn earlier_keyword_case(scan: &Scan, dialect: Dialect) -> Option<Case> {
    let vocabulary = Vocabulary::of(dialect);
    scan.tokens[..scan.at].iter().rev().find_map(|token| {
        let text = token.text(scan.sql);
        (token.kind == TokenKind::Word && vocabulary.is(text, WordClass::Keyword))
            .then(|| Case::of(text))
            .flatten()
    })
}

/// A source order within a kind: shorter names first, then by name.
fn by_length(name: &str, ix: usize) -> u64 {
    ((name.chars().count() as u64) << 32) | ix as u64
}

impl Collector<'_> {
    fn matches(&self, name: &str, lower: &str, loose: bool) -> Option<Grade> {
        Some(Grade {
            tier: tier(&self.query, name, lower, loose)?,
            exact: !self.query.is_empty() && lower == self.query,
            same_case: name.starts_with(self.typed),
        })
    }

    fn push(&mut self, grade: Grade, rank: u8, boost: u8, order: u64, item: Item) {
        let key = (
            grade.tier,
            rank,
            boost,
            !grade.exact,
            !grade.same_case,
            order,
        );
        self.found.push((key, item));
    }

    /// A name as it is inserted: in the quotes the user opened, or quoted
    /// as the dialect needs.
    fn quoted(&self, name: &str) -> String {
        match self.quote {
            Some(open) => {
                let close = if open == '[' { ']' } else { open };
                let escaped = if close == ']' {
                    name.to_owned()
                } else {
                    name.replace(close, &format!("{close}{close}"))
                };
                format!("{open}{escaped}{close}")
            }
            None => self.dialect.quote_ident(name),
        }
    }

    fn ctes(&mut self, ctes: &[String], rank: u8) {
        for (ix, name) in ctes.iter().enumerate() {
            if let Some(grade) = self.matches(name, &name.to_lowercase(), true) {
                let item = Item {
                    label: name.clone(),
                    insert: self.quoted(name),
                    kind: ItemKind::Cte,
                    detail: ItemKind::Cte.label().to_owned(),
                };
                self.push(grade, rank, 0, ix as u64, item);
            }
        }
    }

    /// Every relation in the catalog, named so it resolves from where the
    /// connection starts.
    fn relations(&mut self, rank: u8) {
        let catalog = self.catalog;
        for (ix, relation) in catalog.relations().iter().enumerate() {
            let Some(grade) = self.matches(&relation.name, &relation.lower, true) else {
                continue;
            };
            let insert = if self.quote.is_some() {
                self.quoted(&relation.name)
            } else {
                catalog.qualified(ix as u32, self.dialect)
            };
            let item = self.relation_item(ix as u32, insert);
            self.push(grade, rank, 0, by_length(&relation.name, ix), item);
        }
    }

    fn relation_item(&self, ix: u32, insert: String) -> Item {
        let relation = self.catalog.relation(ix);
        let kind = relation_kind(relation.kind);
        Item {
            label: relation.name.clone(),
            insert,
            kind,
            detail: format!("{} · {}", kind.label(), self.catalog.namespace(ix)),
        }
    }

    fn schemas(&mut self, rank: u8) {
        let catalog = self.catalog;
        for (ix, schema) in catalog.schemas().iter().enumerate() {
            if schema.name.is_empty() {
                continue;
            }
            let Some(grade) = self.matches(&schema.name, &schema.lower, true) else {
                continue;
            };
            let insert = if self.quote.is_some() {
                self.quoted(&schema.name)
            } else {
                catalog.qualified_schema(ix as u32, self.dialect)
            };
            let detail = if catalog.several_databases() {
                format!("schema · {}", catalog.database(schema.database).name)
            } else {
                "schema".to_owned()
            };
            let item = Item {
                label: schema.name.clone(),
                insert,
                kind: ItemKind::Schema,
                detail,
            };
            self.push(grade, rank, 0, by_length(&schema.name, ix), item);
        }
    }

    fn databases(&mut self, rank: u8) {
        if !self.catalog.several_databases() {
            return;
        }
        let catalog = self.catalog;
        for (ix, database) in catalog.databases().iter().enumerate() {
            let Some(grade) = self.matches(&database.name, &database.lower, true) else {
                continue;
            };
            let item = Item {
                label: database.name.clone(),
                insert: self.quoted(&database.name),
                kind: ItemKind::Database,
                detail: ItemKind::Database.label().to_owned(),
            };
            self.push(grade, rank, 0, by_length(&database.name, ix), item);
        }
    }

    /// The catalog relation a statement's reference means, unless it names
    /// one of the statement's own `WITH` queries.
    fn resolve(&self, table: &TableRef, ctes: &[String]) -> Option<u32> {
        if let [name] = table.parts.as_slice()
            && ctes.iter().any(|cte| cte.eq_ignore_ascii_case(name))
        {
            return None;
        }
        self.catalog.find_relations(&table.parts).first().copied()
    }

    /// The columns of the relations in scope, in the statement's order.
    /// A column name more than one of them has is inserted with its table's
    /// alias or name in front, as SQL needs it to be.
    fn scope_columns(&mut self, scope: &[TableRef], ctes: &[String], rank: u8) {
        let catalog = self.catalog;
        let mut seen = HashSet::new();
        let resolved: Vec<(&TableRef, u32)> = scope
            .iter()
            .filter_map(|table| Some((table, self.resolve(table, ctes)?)))
            .filter(|(table, ix)| seen.insert((table.handle().to_lowercase(), *ix)))
            .collect();
        let mut owners: HashMap<&str, usize> = HashMap::new();
        for (_, ix) in &resolved {
            for column in catalog.columns(*ix) {
                *owners.entry(column.lower.as_str()).or_default() += 1;
            }
        }
        for (place, (table, ix)) in resolved.iter().enumerate() {
            let relation = catalog.relation(*ix);
            for (ordinal, column) in catalog.columns(*ix).iter().enumerate() {
                let Some(grade) = self.matches(&column.name, &column.lower, true) else {
                    continue;
                };
                let name = self.quoted(&column.name);
                let insert = if owners[column.lower.as_str()] > 1 && self.quote.is_none() {
                    format!("{}.{name}", self.dialect.quote_ident(table.handle()))
                } else {
                    name
                };
                let owner = match &table.alias {
                    Some(alias) => format!("{} {alias}", relation.name),
                    None => relation.name.clone(),
                };
                let item = Item {
                    label: column.name.clone(),
                    insert,
                    kind: ItemKind::Column,
                    detail: format!("{} · {owner}", column.data_type),
                };
                self.push(
                    grade,
                    rank,
                    0,
                    ((place as u64) << 32) | ordinal as u64,
                    item,
                );
            }
        }
    }

    /// The aliases and table names the statement uses, to qualify a column.
    fn handles(&mut self, scope: &[TableRef], ctes: &[String], rank: u8) {
        let mut seen = HashSet::new();
        for (place, table) in scope.iter().enumerate() {
            let handle = table.handle();
            if handle.is_empty() || !seen.insert(handle.to_lowercase()) {
                continue;
            }
            let Some(grade) = self.matches(handle, &handle.to_lowercase(), true) else {
                continue;
            };
            let relation = self.resolve(table, ctes);
            let (kind, detail) = match (&table.alias, relation) {
                (Some(_), Some(ix)) => (
                    ItemKind::Alias,
                    format!("alias · {}", self.catalog.relation(ix).name),
                ),
                (Some(_), None) => (ItemKind::Alias, ItemKind::Alias.label().to_owned()),
                (None, Some(ix)) => {
                    let kind = relation_kind(self.catalog.relation(ix).kind);
                    (
                        kind,
                        format!("{} · {}", kind.label(), self.catalog.namespace(ix)),
                    )
                }
                (None, None) => (ItemKind::Cte, ItemKind::Cte.label().to_owned()),
            };
            let item = Item {
                label: handle.to_owned(),
                insert: self.quoted(handle),
                kind,
                detail,
            };
            self.push(grade, rank, 0, place as u64, item);
        }
    }

    /// After `a.` or `a.b.`: the columns of the table or alias it names,
    /// else the relations of the schema, else the schemas of the database.
    fn qualified(&mut self, qualifier: &[String], analysis: &Analysis) {
        let catalog = self.catalog;
        let names_relation = analysis.position == Position::Relation;
        if !names_relation {
            if let [handle] = qualifier
                && let Some(table) = analysis
                    .scope
                    .iter()
                    .find(|t| t.handle().eq_ignore_ascii_case(handle))
            {
                if let Some(ix) = self.resolve(table, &analysis.ctes) {
                    self.columns_of(ix, table.alias.as_deref());
                }
                return;
            }
            if let Some(&ix) = catalog.find_relations(qualifier).first() {
                self.columns_of(ix, None);
                return;
            }
        }

        let mut listed = HashSet::new();
        for schema in catalog.find_schemas(qualifier) {
            for ix in catalog.relations_in(schema) {
                listed.insert(ix);
            }
        }
        if let [name] = qualifier
            && let Some(db) = catalog.find_database(name)
        {
            for (ix, schema) in catalog.schemas().iter().enumerate() {
                if schema.database != db || schema.name.is_empty() {
                    continue;
                }
                if let Some(grade) = self.matches(&schema.name, &schema.lower, true) {
                    let item = Item {
                        label: schema.name.clone(),
                        insert: self.quoted(&schema.name),
                        kind: ItemKind::Schema,
                        detail: format!("schema · {}", catalog.database(db).name),
                    };
                    self.push(grade, 1, 0, by_length(&schema.name, ix), item);
                }
                // `db.t` reaches the tables of the database's default schema.
                if !schema.qualify {
                    listed.extend(catalog.relations_in(ix as u32));
                }
            }
        }
        let mut listed: Vec<u32> = listed.into_iter().collect();
        listed.sort_unstable();
        for ix in listed {
            let relation = catalog.relation(ix);
            let Some(grade) = self.matches(&relation.name, &relation.lower, true) else {
                continue;
            };
            let item = self.relation_item(ix, self.quoted(&relation.name));
            self.push(grade, 0, 0, by_length(&relation.name, ix as usize), item);
        }
    }

    fn columns_of(&mut self, ix: u32, alias: Option<&str>) {
        let catalog = self.catalog;
        let relation = catalog.relation(ix);
        let owner = match alias {
            Some(alias) => format!("{} {alias}", relation.name),
            None => relation.name.clone(),
        };
        for (ordinal, column) in catalog.columns(ix).iter().enumerate() {
            let Some(grade) = self.matches(&column.name, &column.lower, true) else {
                continue;
            };
            let item = Item {
                label: column.name.clone(),
                insert: self.quoted(&column.name),
                kind: ItemKind::Column,
                detail: format!("{} · {owner}", column.data_type),
            };
            self.push(grade, 0, 0, ordinal as u64, item);
        }
    }
}

fn relation_kind(kind: RelationKind) -> ItemKind {
    match kind {
        RelationKind::Table => ItemKind::Table,
        RelationKind::View => ItemKind::View,
        RelationKind::MaterializedView => ItemKind::MaterializedView,
    }
}

/// The dialect's words for a flavor, in the case the user writes them.
struct Words<'a> {
    flavor: &'a Flavor,
    keyword_case: Case,
    /// Functions follow only what is typed; untyped they keep the
    /// vocabulary's spelling, as most SQL writes them in lower case.
    function_case: Option<Case>,
}

impl Words<'_> {
    /// Add the words of `class` the release has, with `promoted` ones
    /// before the rest. Keywords only match by prefix: they are short and
    /// many, and a loose match would bury the names.
    fn add(&self, out: &mut Collector, class: WordClass, rank: u8, promoted: &[&str]) {
        let loose = class != WordClass::Keyword;
        let (kind, case) = match class {
            WordClass::Keyword => (ItemKind::Keyword, Some(self.keyword_case)),
            WordClass::Function => (ItemKind::Function, self.function_case),
            WordClass::Type => (ItemKind::Type, Some(self.keyword_case)),
        };
        for (ix, word) in words::of(self.flavor.dialect).iter().enumerate() {
            if word.entry.class != class || !word.available(self.flavor) {
                continue;
            }
            let Some(grade) = out.matches(word.entry.spelling, word.lower, loose) else {
                continue;
            };
            let boost = promoted
                .iter()
                .position(|p| *p == word.lower)
                .map_or(u8::MAX, |p| p as u8);
            let item = word_item(word, kind, case);
            out.push(grade, rank, boost, by_length(word.lower, ix), item);
        }
    }
}

fn word_item(word: &VocabWord, kind: ItemKind, case: Option<Case>) -> Item {
    let spelled = words::spell(word, case);
    Item {
        label: spelled.clone(),
        insert: spelled,
        kind,
        detail: kind.label().to_owned(),
    }
}
