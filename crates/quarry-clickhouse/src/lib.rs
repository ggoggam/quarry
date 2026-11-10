//! ClickHouse driver for Quarry.
//!
//! Statements go over ClickHouse's HTTP interface, one POST each, inside an
//! HTTP session so `SET`, `USE` and temporary tables last between them.
//! Results come back as an Arrow IPC stream (`default_format=ArrowStream`)
//! and are decoded as the bytes arrive, so the first rows reach the grid
//! while the server is still producing the rest.
//!
//! ClickHouse changes from release to release: settings appear, defaults
//! flip, the Arrow writer learns new types. The connection reads the
//! server's version once and [`gates`] decides, per release, which settings
//! to send and which column types to ask the server to render as text.

mod catalog;
mod config;
mod convert;
mod error;
pub mod gates;
mod http;
mod statement;
mod stream;

use std::io::Read as _;
use std::sync::Arc;

use anyhow::Context as _;
use arrow::array::UInt64Array;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use quarry_core::catalog::Catalog;
use quarry_core::result::{empty_schema, native_type};
use quarry_core::{BatchSink, Canceller, Dialect, Driver, Flavor};

use crate::config::Config;
use crate::gates::Treatment;
use crate::http::Http;
use crate::statement::{Kind, Statement};

/// A connection to a ClickHouse server over its HTTP interface.
pub struct ClickHouseDriver {
    http: Http,
    flavor: Flavor,
    /// The session's current database, sent with every statement so a
    /// session the server expired comes back in the right place.
    database: String,
    /// `session_timeout` to ask for; dropped if the server refuses it.
    session_timeout: Option<u32>,
    /// False for `readonly = 1` users, who may not send settings: results
    /// are then asked for with a `FORMAT` clause instead.
    settings_allowed: bool,
}

/// Seconds an idle session lives on the server: ClickHouse's default cap
/// (`max_session_timeout`).
const SESSION_TIMEOUT: u32 = 3600;

impl ClickHouseDriver {
    /// `clickhouse://user:pass@host:8123/db` (HTTP), `clickhouses://...` or
    /// `?secure=true` (HTTPS, default port 8443), and plain `http(s)://` URLs.
    pub fn connect(url: &str) -> anyhow::Result<Self> {
        let config = Config::parse(url)?;
        let target = config.display();
        let mut driver = Self {
            database: config.database.clone(),
            http: Http::new(config),
            flavor: Flavor::unversioned(Dialect::ClickHouse),
            session_timeout: Some(SESSION_TIMEOUT),
            settings_allowed: true,
        };
        const HELLO: &str = "SELECT version(), getSetting('readonly')";
        let hello = match driver.internal(HELLO) {
            Err(err)
                if error::error_code(&err.to_string()) == Some(error::INVALID_SESSION_TIMEOUT) =>
            {
                driver.session_timeout = None;
                driver.internal(HELLO)
            }
            other => other,
        }
        .with_context(|| format!("connecting to {target}"))?;
        let mut fields = hello.trim_end().split('\t');
        let version = fields.next().unwrap_or_default();
        driver.flavor = Flavor::new(Dialect::ClickHouse, version);
        driver.settings_allowed = fields.next() != Some("1");
        Ok(driver)
    }

    /// Settings every request carries.
    fn base_params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![("database", self.database.clone())];
        if let Some(timeout) = self.session_timeout {
            params.push(("session_timeout", timeout.to_string()));
        }
        params
    }

    /// Run an internal statement and return its output, TabSeparated (the
    /// HTTP interface's default, so no setting is needed).
    fn internal(&mut self, sql: &str) -> anyhow::Result<String> {
        let params = self.base_params();
        self.http.text(sql, &params)
    }

    /// Column names and ClickHouse types of a query's result, without
    /// running it. `None` when the server cannot tell.
    fn describe(&mut self, query: &str) -> Option<Vec<(String, String)>> {
        let sql = format!("DESCRIBE TABLE (\n{query}\n)");
        match self.internal(&sql) {
            Ok(text) => Some(
                text.lines()
                    .filter_map(|line| {
                        let mut fields = line.split('\t').map(stream::unescape_tsv);
                        Some((fields.next()?, fields.next()?))
                    })
                    .collect(),
            ),
            Err(err) => {
                log::debug!("could not describe the result: {err:#}");
                None
            }
        }
    }

    fn insert(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        let mut params = self.base_params();
        // Wait for the end so the summary header counts every row.
        params.push(("wait_end_of_query", "1".into()));
        let reply = self.http.send(sql, &params)?;
        let written = reply
            .header("X-ClickHouse-Summary")
            .and_then(|s| http::summary_value(s, "written_rows"))
            .unwrap_or(0);
        let mut rest = Vec::new();
        let read = reply
            .response
            .into_body()
            .into_reader()
            .read_to_end(&mut rest);
        self.http.finish();
        read?;
        let field = native_type(
            Field::new("rows affected", DataType::UInt64, false),
            "UInt64",
        );
        let schema = Arc::new(Schema::new(vec![field]));
        sink.schema(schema.clone());
        let batch = RecordBatch::try_new(schema, vec![Arc::new(UInt64Array::from(vec![written]))])?;
        let _ = sink.batch(batch);
        Ok(())
    }

    fn run(&mut self, statement: &Statement, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        // Ask for the result's real types first, so headers can show them
        // and types this release's Arrow writer handles badly can be sent
        // as text instead.
        let described = match (statement.kind, &statement.format) {
            (Kind::Query, None) => self.describe(statement.text),
            _ => None,
        };
        let mut sql = statement.text.to_owned();
        if let Some(columns) = &described {
            let treatments: Vec<Treatment> = columns
                .iter()
                .map(|(_, ty)| gates::treatment(&self.flavor, ty, self.settings_allowed))
                .collect();
            if let Some(rewritten) = gates::rewrite(statement.text, columns, &treatments) {
                sql = rewritten;
            }
        }

        let mut params = self.base_params();
        if self.settings_allowed {
            params.extend(gates::output_settings(&self.flavor));
        } else if statement.format.is_none() && statement.returns_rows() {
            sql.push_str("\nFORMAT ArrowStream");
        }
        let reply = self.http.send(&sql, &params)?;
        let shared = self.http.shared.clone();
        let query_id = reply.query_id.clone();
        let outcome = stream::read_reply(reply, described.as_deref(), sink);
        self.http.finish();
        match outcome {
            Ok(stream::Ended::Complete) => Ok(()),
            Ok(stream::Ended::Stopped) => {
                // The fetch stopped early; stop the query too, so it does
                // not keep working (and holding the session) unseen.
                std::thread::spawn(move || shared.kill(&query_id, false));
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

impl Driver for ClickHouseDriver {
    fn flavor(&self) -> Flavor {
        self.flavor.clone()
    }

    fn target(&self) -> String {
        self.http.config.display()
    }

    fn execute(&mut self, sql: &str, sink: &mut dyn BatchSink) -> anyhow::Result<()> {
        self.http.begin();
        let statement = Statement::analyze(sql);
        if statement.text.is_empty() {
            sink.schema(empty_schema());
            return Ok(());
        }
        match statement.kind {
            Kind::Insert => self.insert(statement.text, sink),
            Kind::Use => {
                self.run(&statement, sink)?;
                // The database parameter would undo the USE on the next
                // statement, so follow it.
                let mut params = Vec::new();
                if let Some(timeout) = self.session_timeout {
                    params.push(("session_timeout", timeout.to_string()));
                }
                let current = self.http.text("SELECT currentDatabase()", &params)?;
                self.database = stream::unescape_tsv(current.trim_end_matches('\n'));
                Ok(())
            }
            _ => self.run(&statement, sink),
        }
    }

    fn catalog(&mut self) -> anyhow::Result<Catalog> {
        self.http.begin();
        let flavor = self.flavor.clone();
        catalog::read(&flavor, |sql| self.internal(sql))
    }

    fn canceller(&self) -> Canceller {
        let shared = self.http.shared.clone();
        Arc::new(move || shared.cancel())
    }
}
