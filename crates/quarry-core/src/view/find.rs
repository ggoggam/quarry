//! Find in results: which cells of a view show the text searched for.
//!
//! A cell matches on its full text, as the value viewer shows it (not the
//! grid's cut, one-line form); NULL cells never match. Text columns are read
//! in place; other cells are formatted one at a time into a reused buffer.
//! Searching walks the view in order, a range of rows at a time, so a caller
//! can search a large result in steps and stop when the query changes.

use std::ops::Range;

use arrow::array::{Array, ArrayRef, AsArray as _};
use arrow::datatypes::DataType;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use regex::Regex;

use crate::ResultSet;
use crate::result::CellText;

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct FindQuery {
    pub text: String,
    pub case_sensitive: bool,
    /// The whole cell must be the text, not just contain it.
    pub whole_cell: bool,
}

/// A compiled [`FindQuery`].
#[derive(Debug, Clone)]
pub struct Needle {
    matcher: Matcher,
}

#[derive(Debug, Clone)]
enum Matcher {
    Contains(String),
    Equals(String),
    Pattern(Regex),
}

impl Needle {
    /// `None` for an empty query, which finds nothing.
    pub fn new(query: &FindQuery) -> Option<Self> {
        if query.text.is_empty() {
            return None;
        }
        let matcher = match (query.case_sensitive, query.whole_cell) {
            (true, false) => Matcher::Contains(query.text.clone()),
            (true, true) => Matcher::Equals(query.text.clone()),
            (false, whole) => {
                let literal = regex::escape(&query.text);
                let pattern = if whole {
                    format!("(?i)^(?:{literal})$")
                } else {
                    format!("(?i){literal}")
                };
                Matcher::Pattern(Regex::new(&pattern).ok()?)
            }
        };
        Some(Self { matcher })
    }

    /// Matches text containing `text`.
    pub fn contains(text: &str, case_sensitive: bool) -> Self {
        Self::new(&FindQuery {
            text: text.to_owned(),
            case_sensitive,
            whole_cell: false,
        })
        .unwrap_or(Self {
            matcher: Matcher::Contains(String::new()),
        })
    }

    pub fn is_match(&self, text: &str) -> bool {
        match &self.matcher {
            Matcher::Contains(needle) => text.contains(needle.as_str()),
            Matcher::Equals(needle) => text == needle,
            Matcher::Pattern(regex) => regex.is_match(text),
        }
    }
}

/// The text of a string array's value, if `array` is one.
fn string_value(array: &dyn Array, row: usize) -> Option<&str> {
    match array.data_type() {
        DataType::Utf8 => Some(array.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => Some(array.as_string::<i64>().value(row)),
        DataType::Utf8View => Some(array.as_string_view().value(row)),
        _ => None,
    }
}

/// Mark in `hits` the rows of `array` whose text matches `needle`.
pub(crate) fn matches_in(array: &ArrayRef, needle: &Needle, hits: &mut [bool]) {
    let array = super::decode(array);
    if *array.data_type() == DataType::Null {
        return;
    }
    if super::is_string(array.data_type()) {
        for (row, hit) in hits.iter_mut().enumerate().take(array.len()) {
            if !*hit
                && array.is_valid(row)
                && string_value(array.as_ref(), row).is_some_and(|text| needle.is_match(text))
            {
                *hit = true;
            }
        }
        return;
    }
    let options = FormatOptions::default().with_display_error(true);
    let Ok(formatter) = ArrayFormatter::try_new(array.as_ref(), &options) else {
        return;
    };
    let mut text = String::new();
    for (row, hit) in hits.iter_mut().enumerate().take(array.len()) {
        if *hit || array.is_null(row) {
            continue;
        }
        use std::fmt::Write as _;
        text.clear();
        let _ = write!(text, "{}", formatter.value(row));
        if needle.is_match(&text) {
            *hit = true;
        }
    }
}

/// A matching cell: its row in the view and its data column.
pub type Match = (u32, u32);

/// Search rows `range` of the view (`view[i]` is the result row shown at
/// `i`; `None` is the result's own order) in every column, left to right,
/// top to bottom. Matches go to `out` while it holds fewer than `cap`; the
/// return value counts them all.
pub fn find_cells(
    cells: &mut CellText<'_>,
    view: Option<&[u32]>,
    range: Range<usize>,
    needle: &Needle,
    out: &mut Vec<Match>,
    cap: usize,
) -> usize {
    let result: &ResultSet = cells.result();
    let columns = result.column_count();
    let mut text = String::new();
    let mut found = 0;
    for at in range {
        let row = match view {
            Some(view) => match view.get(at) {
                Some(&row) => row as usize,
                None => break,
            },
            None => at,
        };
        let Some((batch, local)) = result.locate(row) else {
            break;
        };
        let arrays = result.batches()[batch].columns();
        for (col, array) in arrays.iter().enumerate().take(columns) {
            if array.is_null(local) {
                continue;
            }
            let hit = match string_value(array.as_ref(), local) {
                Some(value) => needle.is_match(value),
                None => {
                    text.clear();
                    cells.write_at(batch, local, col, &mut text) && needle.is_match(&text)
                }
            };
            if hit {
                found += 1;
                if out.len() < cap {
                    out.push((at as u32, col as u32));
                }
            }
        }
    }
    found
}

/// The match to go to from the cell at `from` (a view row and data
/// column): the first after it going `forward`, else the last before it,
/// wrapping around. Without `from`, the first (or last) match.
pub fn step(matches: &[Match], from: Option<Match>, forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let Some(from) = from else {
        return Some(if forward { 0 } else { matches.len() - 1 });
    };
    if forward {
        let after = matches.partition_point(|m| *m <= from);
        Some(if after == matches.len() { 0 } else { after })
    } else {
        let before = matches.partition_point(|m| *m < from);
        Some(if before == 0 {
            matches.len() - 1
        } else {
            before - 1
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::tests::people;

    fn find(query: FindQuery, view: Option<&[u32]>) -> Vec<Match> {
        let result = people();
        let mut cells = CellText::new(&result);
        let needle = Needle::new(&query).unwrap();
        let mut out = Vec::new();
        let rows = view.map_or(result.row_count(), <[u32]>::len);
        let found = find_cells(&mut cells, view, 0..rows, &needle, &mut out, usize::MAX);
        assert_eq!(found, out.len());
        out
    }

    fn q(text: &str, case_sensitive: bool, whole_cell: bool) -> FindQuery {
        FindQuery {
            text: text.into(),
            case_sensitive,
            whole_cell,
        }
    }

    #[test]
    fn finds_text_with_or_without_case() {
        // (id, name, score): 0: 3 carol 2.5 · 1: 1 NULL -1 · 2: 2 Bob 2.5
        // 3: NULL alice NaN · 4: 10 dave NULL · 5: -4 bob 0
        assert_eq!(find(q("bob", false, false), None), [(2, 1), (5, 1)]);
        assert_eq!(find(q("bob", true, false), None), [(5, 1)]);
        assert_eq!(find(q("BOB", false, true), None), [(2, 1), (5, 1)]);
        assert_eq!(find(q("bo", false, true), None), []);
        assert_eq!(find(q("2.5", true, true), None), [(0, 2), (2, 2)]);
        assert_eq!(find(q("1", true, false), None), [(1, 0), (1, 2), (4, 0)]);
        // Regex characters are only text.
        assert_eq!(
            find(q(".", false, false), None),
            [(0, 2), (1, 2), (2, 2), (5, 2)]
        );
        // NULLs never match, not even the word.
        assert_eq!(find(q("null", false, false), None), []);
        assert!(Needle::new(&q("", false, false)).is_none());
    }

    #[test]
    fn walks_the_view_in_its_order() {
        let view = [5u32, 2, 0];
        assert_eq!(find(q("b", false, false), Some(&view)), [(0, 1), (1, 1)]);
    }

    #[test]
    fn stops_storing_at_the_cap_but_keeps_counting() {
        let result = people();
        let mut cells = CellText::new(&result);
        let needle = Needle::new(&q("a", false, false)).unwrap();
        let mut out = Vec::new();
        let found = find_cells(&mut cells, None, 0..6, &needle, &mut out, 2);
        assert_eq!((found, out.len()), (4, 2));
        // Searching in steps finds the same as at once.
        let mut stepped = Vec::new();
        let mut cells = CellText::new(&result);
        for range in [0..2, 2..5, 5..9] {
            find_cells(&mut cells, None, range, &needle, &mut stepped, usize::MAX);
        }
        assert_eq!(stepped.len(), 4);
    }

    #[test]
    fn steps_between_matches_and_wraps() {
        let matches = [(1, 0), (1, 3), (4, 1)];
        assert_eq!(step(&matches, None, true), Some(0));
        assert_eq!(step(&matches, None, false), Some(2));
        assert_eq!(step(&matches, Some((1, 0)), true), Some(1));
        assert_eq!(step(&matches, Some((1, 1)), true), Some(1));
        assert_eq!(step(&matches, Some((4, 1)), true), Some(0));
        assert_eq!(step(&matches, Some((1, 3)), false), Some(0));
        assert_eq!(step(&matches, Some((0, 0)), false), Some(2));
        assert_eq!(step(&[], None, true), None);
    }
}
