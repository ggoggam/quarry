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

pub mod catalog;
pub mod dialect;
pub mod highlight;
pub mod lex;
pub mod result;
pub mod session;
pub mod sql;
pub mod vocabulary;

pub use arrow;
pub use catalog::Catalog;
pub use dialect::{Dialect, Flavor, Version};
pub use result::ResultSet;
pub use session::{
    BatchSink, Canceller, Driver, QueryEvent, QueryId, QueryOptions, QuerySummary, RunningQuery,
    Session,
};
