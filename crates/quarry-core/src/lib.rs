//! Engine-agnostic building blocks for Quarry.
//!
//! - [`session`]: a connection on its own thread, streaming Arrow batches.
//! - [`result`]: result sets kept columnar and formatted only when drawn.
//! - [`catalog`]: the objects a connection exposes, for the schema browser.
//! - [`dialect`]: SQL dialects and engine versions.
//! - [`lex`]: a dialect-aware SQL tokenizer.
//! - [`highlight`]: syntax highlighting and version notes for a flavor.
//! - [`sql`]: statement splitting for "run statement at caret".
//! - [`vocabulary`]: each dialect's keywords, functions and types, by release.
//! - [`view`]: sorting, filtering and finding over a result, as row indices.
//! - [`export`]: writing a result as CSV, TSV, JSON, Markdown or SQL INSERTs.
//! - [`json`]: telling JSON values apart and laying them out.

pub mod catalog;
pub mod dialect;
pub mod export;
pub mod highlight;
pub mod json;
pub mod lex;
pub mod result;
pub mod session;
pub mod sql;
pub mod view;
pub mod vocabulary;

pub use arrow;
pub use catalog::Catalog;
pub use dialect::{Dialect, Flavor, Release, Version};
pub use result::ResultSet;
pub use session::{
    BatchSink, Canceller, Driver, QueryEvent, QueryId, QueryOptions, QuerySummary, RunningQuery,
    Session,
};
