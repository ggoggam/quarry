//! The SQL editor's highlighter and version notes, for the connected flavor.
//!
//! The editor's own highlighter parses SQL with one generic tree-sitter
//! grammar, which gives up on dialect syntax (a MySQL `#` comment turns the
//! rest of the statement into an error node). This one highlights from
//! [`quarry_core::highlight`] instead: the dialect's own tokenizer, and the
//! keywords, functions and types of the connected release.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::base::input::{
    Diagnostic, DiagnosticSeverity, EditorState, FoldRange, HighlightStyleResolver, InputEdit,
    InputHighlighter, InputHighlighterFactory, Rope, RopeExt as _,
};
use gpui_kit::{Context, HighlightStyle, SharedString, Window};
use quarry_core::Flavor;
use quarry_core::highlight::{Span, analyze};

/// A highlighter factory for an editor showing `flavor`'s SQL.
pub fn factory(flavor: Flavor) -> InputHighlighterFactory {
    Rc::new(move |_language| Some(Box::new(SqlHighlighter::new(flavor.clone()))))
}

struct SqlHighlighter {
    flavor: Flavor,
    language: SharedString,
    spans: Vec<Span>,
}

impl SqlHighlighter {
    fn new(flavor: Flavor) -> Self {
        Self {
            language: format!("sql-{}", flavor.dialect.id()).into(),
            flavor,
            spans: Vec::new(),
        }
    }

    fn reparse(&mut self, text: &Rope) {
        self.spans = analyze(&text.to_string(), &self.flavor).spans;
    }
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // Tokenizing is linear and cheap next to layout, so every change
        // retokenizes the whole text rather than patching spans.
        self.reparse(text);
    }

    fn update_batch(
        &mut self,
        edits: &[(InputEdit, Rope)],
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        if let Some((_, text)) = edits.last() {
            self.reparse(text);
        }
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        style_runs(&self.spans, range, |span| {
            resolver
                .style(span.highlight.theme_name())
                .unwrap_or_default()
        })
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

/// Ordered, non-overlapping runs that cover `range` exactly, with `style`
/// for highlighted spans and the default style between them.
fn style_runs<S: Default + Clone>(
    spans: &[Span],
    range: &Range<usize>,
    style: impl Fn(&Span) -> S,
) -> Vec<(Range<usize>, S)> {
    let mut runs = Vec::new();
    let mut at = range.start;
    let first = spans.partition_point(|s| s.range.end <= range.start);
    for span in &spans[first..] {
        if span.range.start >= range.end {
            break;
        }
        let start = span.range.start.max(range.start);
        let end = span.range.end.min(range.end);
        if start > at {
            runs.push((at..start, S::default()));
        }
        runs.push((start..end, style(span)));
        at = end;
    }
    if at < range.end {
        runs.push((at..range.end, S::default()));
    }
    runs
}

/// Mark the words in the editor that `flavor`'s release does not support.
pub fn update_version_notes(
    editor: &mut EditorState,
    flavor: &Flavor,
    cx: &mut Context<EditorState>,
) {
    let text = editor.text().clone();
    let notes = analyze(&text.to_string(), flavor).notes;
    let Some(diagnostics) = editor.diagnostics_mut() else {
        return;
    };
    diagnostics.reset(&text);
    for note in notes {
        let range =
            text.offset_to_position(note.range.start)..text.offset_to_position(note.range.end);
        diagnostics.push(
            Diagnostic::new(range, note.message)
                .with_severity(DiagnosticSeverity::Warning)
                .with_source(flavor.label()),
        );
    }
    cx.notify();
}

#[cfg(test)]
mod tests {
    use quarry_core::Dialect;
    use quarry_core::highlight::Highlight;

    use super::*;

    #[test]
    fn runs_cover_the_requested_range() {
        let flavor = Flavor::new(Dialect::Postgres, "17.0");
        let sql = "SELECT 1 -- x";
        let spans = analyze(sql, &flavor).spans;
        let runs = style_runs(&spans, &(3..sql.len()), |s| Some(s.highlight));
        assert_eq!(
            runs,
            [
                (3..6, Some(Highlight::Keyword)),
                (6..7, None),
                (7..8, Some(Highlight::Number)),
                (8..9, None),
                (9..13, Some(Highlight::Comment)),
            ]
        );
        assert_eq!(style_runs(&spans, &(0..0), |_| Some(())), []);
        assert_eq!(style_runs(&[], &(0..4), |_| Some(())), [(0..4, None::<()>)]);
    }
}
