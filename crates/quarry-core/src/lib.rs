//! Engine-agnostic building blocks for Quarry.
//!
//! - [`session`]: a connection on its own thread, streaming Arrow batches.
//! - [`result`]: result sets kept columnar and formatted only when drawn.
//! - [`catalog`]: the objects a connection exposes, for the schema browser.
//! - [`sql`]: statement splitting for "run statement at caret".

pub mod catalog;
pub mod result;
pub mod session;
pub mod sql;

pub use arrow;
pub use catalog::Catalog;
pub use result::ResultSet;
pub use session::{
    BatchSink, Canceller, Driver, QueryEvent, QueryId, QueryOptions, QuerySummary, RunningQuery,
    Session,
};
