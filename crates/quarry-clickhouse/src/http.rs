//! ClickHouse's HTTP interface.

use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use base64::Engine as _;
use ureq::Agent;
use ureq::http::Response;

use crate::config::Config;
use crate::error::{self, SESSION_IS_LOCKED};

pub(crate) type Body = ureq::Body;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a statement waits for the session to be free of an earlier,
/// cancelled query before giving up.
const SESSION_LOCK_WAIT: Duration = Duration::from_secs(15);

/// A random-enough id for queries and sessions.
pub(crate) fn random_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::hash::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    let a = hasher.finish();
    hasher.write_u64(a);
    format!("{prefix}-{a:016x}{:016x}", hasher.finish())
}

fn new_agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(concat!("quarry/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// What the canceller needs, shared with the driver's thread.
pub(crate) struct Shared {
    endpoint: String,
    auth: String,
    /// For `KILL QUERY`, which must not wait behind the statement it stops.
    control: Agent,
    running: Mutex<Option<Running>>,
    /// Set when a cancel shut the statement agent down.
    agent_dead: AtomicBool,
    /// Set by a cancel, cleared when the next statement starts: stops the
    /// steps of a statement that had not reached the server yet.
    cancelled: AtomicBool,
}

#[derive(Clone)]
struct Running {
    query_id: String,
    agent: Agent,
}

impl Shared {
    /// Stop the running statement: kill it on the server and abort the
    /// read. Returns at once; the kill is sent from another thread.
    pub fn cancel(self: &Arc<Self>) {
        self.cancelled.store(true, Ordering::Release);
        let Some(running) = self.running.lock().expect("lock").clone() else {
            return;
        };
        self.agent_dead.store(true, Ordering::Release);
        running.agent.shutdown();
        let shared = self.clone();
        thread::spawn(move || shared.kill(&running.query_id, true));
    }

    /// Ask the server to stop `query_id`. With `retry`, try again for a
    /// moment when nothing was killed, since a cancel can arrive before the
    /// server has registered the query.
    pub fn kill(&self, query_id: &str, retry: bool) {
        let sql = format!(
            "KILL QUERY WHERE query_id = '{}' ASYNC FORMAT TabSeparated",
            query_id.replace('\\', "\\\\").replace('\'', "\\'")
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let killed = self
                .control
                .post(&self.endpoint)
                .header("Authorization", &self.auth)
                .send(sql.as_bytes())
                .ok()
                .filter(|r| r.status().is_success())
                .and_then(|r| r.into_body().read_to_string().ok())
                .is_some_and(|text| !text.trim().is_empty());
            if killed || !retry || Instant::now() >= deadline {
                if !killed {
                    log::debug!("KILL QUERY found nothing to stop for {query_id}");
                }
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// A ClickHouse server reached over HTTP(S), with one session.
pub(crate) struct Http {
    pub config: Config,
    agent: Agent,
    session_id: String,
    pub shared: Arc<Shared>,
}

/// A response whose status said success.
pub(crate) struct Reply {
    pub query_id: String,
    pub response: Response<Body>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response.headers().get(name)?.to_str().ok()
    }
}

impl Http {
    pub fn new(config: Config) -> Self {
        let credentials = format!("{}:{}", config.user, config.password);
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        );
        Self {
            shared: Arc::new(Shared {
                endpoint: config.endpoint(),
                auth,
                control: new_agent(),
                running: Mutex::new(None),
                agent_dead: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
            }),
            agent: new_agent(),
            session_id: random_id("quarry-session"),
            config,
        }
    }

    /// Run `sql` with `params` (settings and HTTP options) in this
    /// connection's session. Errors carry ClickHouse's cleaned message.
    pub fn send(&mut self, sql: &str, params: &[(&str, String)]) -> anyhow::Result<Reply> {
        if self.shared.agent_dead.swap(false, Ordering::AcqRel) {
            self.agent = new_agent();
        }
        if self.shared.cancelled.load(Ordering::Acquire) {
            anyhow::bail!("cancelled");
        }
        let query_id = random_id("quarry");
        let started = Instant::now();
        loop {
            *self.shared.running.lock().expect("lock") = Some(Running {
                query_id: query_id.clone(),
                agent: self.agent.clone(),
            });
            let result = self.post(sql, params, &query_id);
            let response = match result {
                Ok(response) => response,
                Err(err) => {
                    self.finish();
                    return Err(err);
                }
            };
            if response.status().is_success() {
                return Ok(Reply { query_id, response });
            }
            self.finish();
            let message = error_body(response);
            // A cancelled query can hold the session for a moment.
            if error::error_code(&message) == Some(SESSION_IS_LOCKED)
                && started.elapsed() < SESSION_LOCK_WAIT
                && !self.shared.cancelled.load(Ordering::Acquire)
            {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            return Err(anyhow!(message));
        }
    }

    /// A new statement starts: an earlier cancel no longer applies.
    pub fn begin(&self) {
        self.shared.cancelled.store(false, Ordering::Release);
    }

    /// The statement is done (or abandoned); a cancel no longer applies.
    pub fn finish(&self) {
        self.shared.running.lock().expect("lock").take();
    }

    fn post(
        &self,
        sql: &str,
        params: &[(&str, String)],
        query_id: &str,
    ) -> anyhow::Result<Response<Body>> {
        let mut request = self
            .agent
            .post(&self.shared.endpoint)
            .header("Authorization", &self.shared.auth)
            .query("query_id", query_id)
            .query("session_id", &self.session_id);
        for (key, value) in &self.config.settings {
            request = request.query(key, value);
        }
        for (key, value) in params {
            request = request.query(*key, value);
        }
        request
            .content_type("text/plain; charset=utf-8")
            .send(sql.as_bytes())
            .map_err(|err| match err {
                ureq::Error::Io(io) => anyhow!("{}: {io}", self.config.endpoint()),
                other => anyhow!("{}: {other}", self.config.endpoint()),
            })
    }

    /// Run a small internal query in this session and read its whole body.
    pub fn text(&mut self, sql: &str, params: &[(&str, String)]) -> anyhow::Result<String> {
        let reply = self.send(sql, params)?;
        let mut body = String::new();
        let result = reply
            .response
            .into_body()
            .into_reader()
            .read_to_string(&mut body);
        self.finish();
        result?;
        Ok(body)
    }
}

/// The error message in a failed response.
fn error_body(response: Response<Body>) -> String {
    let status = response.status();
    let code = response
        .headers()
        .get("X-ClickHouse-Exception-Code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(1 << 20)
        .read_to_end(&mut bytes);
    let text = error::clean_message(&String::from_utf8_lossy(&bytes));
    match (text.is_empty(), code) {
        (false, _) => text,
        (true, Some(code)) => format!("Code: {code}. HTTP {status}"),
        (true, None) => format!("HTTP {status}"),
    }
}

/// `written_rows` from an `X-ClickHouse-Summary` header, e.g.
/// `{"read_rows":"0","written_rows":"2",...}`.
pub(crate) fn summary_value(summary: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\":");
    let at = summary.find(&pattern)? + pattern.len();
    let digits: String = summary[at..]
        .trim_start()
        .trim_start_matches('"')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_summary_values() {
        let summary = r#"{"read_rows":"0","read_bytes":"0","written_rows":"2","written_bytes":"10","total_rows_to_read":"0","result_rows":"2","result_bytes":"10"}"#;
        assert_eq!(summary_value(summary, "written_rows"), Some(2));
        assert_eq!(summary_value(summary, "read_rows"), Some(0));
        assert_eq!(summary_value(summary, "missing"), None);
        assert_eq!(
            summary_value(r#"{"written_rows": 7}"#, "written_rows"),
            Some(7)
        );
    }

    #[test]
    fn makes_distinct_ids() {
        let a = random_id("q");
        let b = random_id("q");
        assert_ne!(a, b);
        assert!(a.starts_with("q-") && a.len() == 2 + 32, "{a}");
    }
}
