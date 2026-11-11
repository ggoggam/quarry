//! One run of statements as plain data: what each statement's
//! [`QueryEvent`]s mean for the results pane (a result tab, or a line in the
//! Output view) and for the query history. No GPUI here, so a run can be
//! driven through a real [`quarry_core::Session`] in tests.

use std::time::Duration;

use quarry_core::arrow::array::{Array as _, AsArray as _};
use quarry_core::arrow::datatypes::{DataType, Int64Type, SchemaRef, UInt64Type};
use quarry_core::arrow::record_batch::RecordBatch;
use quarry_core::{Dialect, QueryEvent, QuerySummary};

use super::tabs::ResultTabId;
use super::title;
use crate::format::{format_elapsed, plural};
use crate::history::{Entry, Outcome};

/// Longest statement preview in an Output line.
const PREVIEW_CHARS: usize = 160;

/// What a statement's result is, judged from its schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Rows to look at: it gets a result tab.
    Rows,
    /// A count of rows changed, which the Output view reports.
    Affected,
    /// Nothing to show beyond that it worked.
    Done,
}

/// Drivers report a statement that returns no rows with an empty schema or
/// a one-column `rows affected` result; DuckDB with a `Count` or `Success`
/// column.
pub fn shape(statement: &str, schema: &SchemaRef, dialect: Dialect) -> Shape {
    let fields = schema.fields();
    let [field] = fields.as_ref() else {
        return if fields.is_empty() {
            Shape::Done
        } else {
            Shape::Rows
        };
    };
    if field.name() == "rows affected" {
        return Shape::Affected;
    }
    if dialect == Dialect::DuckDb && !title::is_query(statement, dialect) {
        match (field.name().as_str(), field.data_type()) {
            ("Count", DataType::Int64) => return Shape::Affected,
            ("Success", DataType::Boolean) => return Shape::Done,
            _ => {}
        }
    }
    Shape::Rows
}

/// How an Output line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Error,
}

/// One line of the Output view: a statement and what came of it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLine {
    /// When the statement started, in milliseconds since the Unix epoch.
    pub at: i64,
    /// The statement on one line, cut.
    pub statement: String,
    pub message: String,
    pub tone: Tone,
    /// The engine's error message.
    pub detail: Option<String>,
    /// The result tab it opened, to show it from here.
    pub result: Option<ResultTabId>,
}

/// What to do with an event of the statement running.
#[derive(Debug)]
pub enum Step {
    Nothing,
    /// The statement returns rows: show them in a result tab.
    Open(SchemaRef),
    /// Rows for the result tab [`Step::Open`] asked for.
    Rows(RecordBatch),
    Finished(Box<Finished>),
}

/// A statement ended.
#[derive(Debug)]
pub struct Finished {
    /// Which of the run's statements it was.
    pub ix: usize,
    pub outcome: Outcome,
    /// It opened a result tab.
    pub opened: bool,
    pub summary: Option<QuerySummary>,
    /// The engine's message, when it failed.
    pub error: Option<String>,
    pub elapsed: Duration,
    pub line: OutputLine,
    pub entry: Entry,
    /// Another statement follows: the run goes on.
    pub more: bool,
}

struct Current {
    ix: usize,
    started_at: i64,
    shape: Option<Shape>,
    affected: Option<u64>,
}

/// The statements of one run, and where it is in them.
pub struct Run {
    statements: Vec<String>,
    dialect: Dialect,
    connection: String,
    current: Option<Current>,
    /// Statements finished, so far.
    done: usize,
    failed: bool,
    cancelled: bool,
}

impl Run {
    /// `connection` is the connection's password-free key, for the history.
    pub fn new(statements: Vec<String>, dialect: Dialect, connection: String) -> Self {
        Self {
            statements,
            dialect,
            connection,
            current: None,
            done: 0,
            failed: false,
            cancelled: false,
        }
    }

    pub fn len(&self) -> usize {
        self.statements.len()
    }

    pub fn statement(&self, ix: usize) -> &str {
        &self.statements[ix]
    }

    /// Statement `ix` and the ones after it.
    pub fn statements_from(&self, ix: usize) -> Vec<String> {
        self.statements[ix.min(self.statements.len())..].to_vec()
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The statement running, if one is.
    pub fn current(&self) -> Option<usize> {
        self.current.as_ref().map(|c| c.ix)
    }

    /// Statements finished so far, and whether one failed or was cancelled.
    pub fn progress(&self) -> (usize, bool, bool) {
        (self.done, self.failed, self.cancelled)
    }

    /// Statement `ix` was sent at `now` (milliseconds since the Unix epoch).
    pub fn start(&mut self, ix: usize, now: i64) {
        self.current = Some(Current {
            ix,
            started_at: now,
            shape: None,
            affected: None,
        });
    }

    pub fn apply(&mut self, event: QueryEvent) -> Step {
        let Some(current) = &mut self.current else {
            return Step::Nothing;
        };
        match event {
            QueryEvent::Schema(schema) => {
                let shape = shape(&self.statements[current.ix], &schema, self.dialect);
                current.shape = Some(shape);
                match shape {
                    Shape::Rows => Step::Open(schema),
                    Shape::Affected | Shape::Done => Step::Nothing,
                }
            }
            QueryEvent::Batch(batch) => match current.shape {
                Some(Shape::Rows) => Step::Rows(batch),
                Some(Shape::Affected) => {
                    current.affected = current.affected.or_else(|| first_count(&batch));
                    Step::Nothing
                }
                _ => Step::Nothing,
            },
            QueryEvent::Finished(summary) => {
                let outcome = if summary.cancelled {
                    Outcome::Cancelled
                } else {
                    Outcome::Succeeded
                };
                self.finish(outcome, Some(summary.clone()), None, summary.elapsed)
            }
            QueryEvent::Failed { message, elapsed } => {
                self.finish(Outcome::Failed, None, Some(message), elapsed)
            }
        }
    }

    /// The run stops before its statement finished: the connection went
    /// away, or another run replaced this one. The statement counts as
    /// cancelled, with the rows that had arrived.
    pub fn abort(&mut self, rows: usize, elapsed: Duration) -> Option<Box<Finished>> {
        self.current.as_ref()?;
        let summary = QuerySummary {
            rows,
            elapsed,
            truncated: false,
            cancelled: true,
        };
        match self.finish(Outcome::Cancelled, Some(summary), None, elapsed) {
            Step::Finished(finished) => Some(finished),
            _ => None,
        }
    }

    fn finish(
        &mut self,
        outcome: Outcome,
        summary: Option<QuerySummary>,
        error: Option<String>,
        elapsed: Duration,
    ) -> Step {
        let Some(current) = self.current.take() else {
            return Step::Nothing;
        };
        let statement = &self.statements[current.ix];
        let opened = current.shape == Some(Shape::Rows);
        let rows = summary.as_ref().map(|s| s.rows).filter(|_| opened);
        let affected = current.affected.filter(|_| outcome == Outcome::Succeeded);
        let took = format_elapsed(elapsed);
        let (message, tone) = match outcome {
            Outcome::Failed => (format!("failed after {took}"), Tone::Error),
            Outcome::Cancelled => {
                let fetched = rows
                    .map(|rows| format!(", {} retrieved", plural(rows, "row")))
                    .unwrap_or_default();
                (format!("cancelled after {took}{fetched}"), Tone::Muted)
            }
            Outcome::Succeeded => {
                let message = match (rows, affected) {
                    (Some(rows), _) => {
                        let mut message = format!("{} retrieved in {took}", plural(rows, "row"));
                        if summary.as_ref().is_some_and(|s| s.truncated) {
                            message.push_str(", stopped at the row limit");
                        }
                        message
                    }
                    (None, Some(affected)) => {
                        format!("{} affected in {took}", plural(affected as usize, "row"))
                    }
                    (None, None) => format!("completed in {took}"),
                };
                (message, Tone::Normal)
            }
        };
        let line = OutputLine {
            at: current.started_at,
            statement: title::preview(statement, PREVIEW_CHARS),
            message,
            tone,
            detail: error.clone(),
            result: None,
        };
        let entry = Entry {
            statement: statement.clone(),
            cut: false,
            connection: self.connection.clone(),
            dialect: self.dialect.id().to_owned(),
            at: current.started_at,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            outcome,
            rows: rows.map(|r| r as u64),
            affected,
            error: error.clone(),
        };
        self.done += 1;
        self.failed |= outcome == Outcome::Failed;
        self.cancelled |= outcome == Outcome::Cancelled;
        let more = outcome == Outcome::Succeeded && current.ix + 1 < self.statements.len();
        Step::Finished(Box::new(Finished {
            ix: current.ix,
            outcome,
            opened,
            summary,
            error,
            elapsed,
            line,
            entry,
            more,
        }))
    }
}

/// The count in a one-row `rows affected` (or DuckDB `Count`) result.
fn first_count(batch: &RecordBatch) -> Option<u64> {
    let column = batch.columns().first()?;
    if column.is_empty() || column.is_null(0) {
        return None;
    }
    match column.data_type() {
        DataType::Int64 => u64::try_from(column.as_primitive::<Int64Type>().value(0)).ok(),
        DataType::UInt64 => Some(column.as_primitive::<UInt64Type>().value(0)),
        _ => None,
    }
}

/// What a whole run came to, for the status bar while its Output shows.
pub fn summary_text(statements: usize, done: usize, failed: bool, cancelled: bool) -> String {
    if failed {
        if statements > 1 {
            format!("Statement {done} of {statements} failed")
        } else {
            "Query failed".to_owned()
        }
    } else if cancelled {
        format!("Cancelled after {} of {statements}", done.saturating_sub(1))
    } else {
        format!("{} run", plural(statements, "statement"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use quarry_core::arrow::array::{BooleanArray, Int64Array};
    use quarry_core::arrow::datatypes::{Field, Schema};
    use quarry_core::{QueryOptions, Session};
    use quarry_duckdb::DuckDbDriver;

    use super::*;

    fn schema(fields: Vec<Field>) -> SchemaRef {
        Arc::new(Schema::new(fields))
    }

    #[test]
    fn tells_rows_from_counts() {
        let duck = Dialect::DuckDb;
        let count = schema(vec![Field::new("Count", DataType::Int64, true)]);
        assert_eq!(
            shape("INSERT INTO t VALUES (1)", &count, duck),
            Shape::Affected
        );
        assert_eq!(shape("SELECT 1 AS \"Count\"", &count, duck), Shape::Rows);
        let success = schema(vec![Field::new("Success", DataType::Boolean, true)]);
        assert_eq!(shape("DROP TABLE t", &success, duck), Shape::Done);
        assert_eq!(
            shape("CREATE TABLE t (a INT)", &schema(vec![]), duck),
            Shape::Done
        );
        let affected = schema(vec![Field::new("rows affected", DataType::Int64, false)]);
        assert_eq!(
            shape("UPDATE t SET a = 1", &affected, Dialect::Postgres),
            Shape::Affected
        );
        // Only DuckDB reports changes as `Count`.
        assert_eq!(shape("DELETE FROM t", &count, Dialect::Sqlite), Shape::Rows);
        let two = schema(vec![
            Field::new("a", DataType::Int64, true),
            Field::new("b", DataType::Int64, true),
        ]);
        assert_eq!(
            shape("INSERT INTO t VALUES (1) RETURNING a, b", &two, duck),
            Shape::Rows
        );
    }

    /// Run `statements` through a DuckDB session the way the app does, and
    /// collect what each one finished with.
    fn run(session: &Session, statements: &[&str]) -> (Vec<Finished>, Vec<usize>) {
        let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
        let mut run = Run::new(statements.clone(), Dialect::DuckDb, ":memory:".into());
        let mut finished = Vec::new();
        let mut grids = Vec::new();
        for (ix, statement) in statements.iter().enumerate() {
            run.start(ix, 1_000 + ix as i64);
            let query = session.query(statement.clone(), QueryOptions::default());
            let mut rows = 0;
            loop {
                let event = query.events.recv().expect("the session answers");
                match run.apply(event) {
                    Step::Nothing => {}
                    Step::Open(_) => grids.push(ix),
                    Step::Rows(batch) => rows += batch.num_rows(),
                    Step::Finished(done) => {
                        if let Some(summary) = done.summary.as_ref().filter(|_| done.opened) {
                            assert_eq!(summary.rows, rows);
                        }
                        let more = done.more;
                        finished.push(*done);
                        if more {
                            break;
                        }
                        return (finished, grids);
                    }
                }
            }
        }
        (finished, grids)
    }

    #[test]
    fn a_script_reports_each_statement() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (finished, grids) = run(
            &session,
            &[
                "CREATE TABLE t (id INTEGER)",
                "INSERT INTO t SELECT * FROM range(5)",
                "SELECT * FROM t",
                "DROP TABLE t",
                "SELECT 42",
            ],
        );
        assert_eq!(grids, [2, 4], "the two SELECTs get result tabs");
        let lines: Vec<_> = finished.iter().map(|f| f.line.message.as_str()).collect();
        assert!(lines[0].starts_with("completed in"), "{lines:?}");
        assert!(lines[1].starts_with("5 rows affected in"), "{lines:?}");
        assert!(lines[2].starts_with("5 rows retrieved in"), "{lines:?}");
        assert!(lines[3].starts_with("completed in"), "{lines:?}");
        assert!(lines[4].starts_with("1 row retrieved in"), "{lines:?}");
        let entry = &finished[1].entry;
        assert_eq!(entry.statement, "INSERT INTO t SELECT * FROM range(5)");
        assert_eq!(entry.connection, ":memory:");
        assert_eq!(entry.dialect, "duckdb");
        assert_eq!(entry.at, 1_001);
        assert_eq!((entry.rows, entry.affected), (None, Some(5)));
        assert_eq!(finished[2].entry.rows, Some(5));
        assert!(finished.iter().all(|f| f.outcome == Outcome::Succeeded));
        assert!(!finished.last().unwrap().more);
    }

    #[test]
    fn a_failure_stops_the_run_with_the_engines_message() {
        let session = Session::spawn(DuckDbDriver::in_memory().unwrap());
        let (finished, grids) = run(&session, &["SELECT 1", "SELECT nope", "SELECT 2"]);
        assert_eq!(finished.len(), 2, "the third statement never runs");
        assert_eq!(grids, [0]);
        let failed = &finished[1];
        assert_eq!(failed.outcome, Outcome::Failed);
        assert_eq!(failed.line.tone, Tone::Error);
        assert!(failed.line.message.starts_with("failed after"));
        let message = failed.error.as_deref().unwrap();
        assert!(message.contains("nope"), "{message}");
        assert_eq!(failed.line.detail.as_deref(), Some(message));
        assert_eq!(failed.entry.error.as_deref(), Some(message));
        assert_eq!(failed.entry.rows, None);
        assert!(!failed.more);
    }

    #[test]
    fn a_cancelled_statement_stops_the_run() {
        let mut run = Run::new(
            vec!["SELECT * FROM big".into(), "SELECT 2".into()],
            Dialect::DuckDb,
            "k".into(),
        );
        run.start(0, 0);
        let columns = schema(vec![Field::new("a", DataType::Int64, true)]);
        assert!(matches!(
            run.apply(QueryEvent::Schema(columns)),
            Step::Open(_)
        ));
        let Step::Finished(done) = run.apply(QueryEvent::Finished(QuerySummary {
            rows: 2048,
            elapsed: Duration::from_millis(1500),
            truncated: false,
            cancelled: true,
        })) else {
            panic!("finished")
        };
        assert_eq!(done.outcome, Outcome::Cancelled);
        assert_eq!(
            done.line.message,
            "cancelled after 1.50 s, 2,048 rows retrieved"
        );
        assert!(!done.more);
        assert_eq!(run.progress(), (1, false, true));
    }

    #[test]
    fn an_aborted_run_records_its_statement_as_cancelled() {
        let mut run = Run::new(
            vec!["UPDATE t SET a = 1".into()],
            Dialect::DuckDb,
            "k".into(),
        );
        assert!(
            run.abort(0, Duration::ZERO).is_none(),
            "nothing running yet"
        );
        run.start(0, 7);
        let count = schema(vec![Field::new("Count", DataType::Int64, true)]);
        run.apply(QueryEvent::Schema(count.clone()));
        let batch = RecordBatch::try_new(count, vec![Arc::new(Int64Array::from(vec![3]))]).unwrap();
        run.apply(QueryEvent::Batch(batch));
        let done = run.abort(0, Duration::from_millis(20)).unwrap();
        assert_eq!(done.entry.outcome, Outcome::Cancelled);
        assert_eq!(
            done.entry.affected, None,
            "a cancelled change reports no count"
        );
        assert_eq!(done.entry.at, 7);
        assert!(run.current().is_none());
    }

    #[test]
    fn ignores_a_success_flag() {
        let mut run = Run::new(vec!["BEGIN".into()], Dialect::DuckDb, "k".into());
        run.start(0, 0);
        let success = schema(vec![Field::new("Success", DataType::Boolean, true)]);
        run.apply(QueryEvent::Schema(success.clone()));
        let batch =
            RecordBatch::try_new(success, vec![Arc::new(BooleanArray::from(vec![true]))]).unwrap();
        assert!(matches!(run.apply(QueryEvent::Batch(batch)), Step::Nothing));
    }

    #[test]
    fn sums_up_a_run() {
        assert_eq!(summary_text(3, 3, false, false), "3 statements run");
        assert_eq!(summary_text(1, 1, false, false), "1 statement run");
        assert_eq!(summary_text(3, 2, true, false), "Statement 2 of 3 failed");
        assert_eq!(summary_text(1, 1, true, false), "Query failed");
        assert_eq!(summary_text(3, 2, false, true), "Cancelled after 1 of 3");
    }
}
