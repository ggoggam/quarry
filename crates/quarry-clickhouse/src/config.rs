//! Connection URLs.

use anyhow::{Context as _, bail};
use url::Url;

/// Where and as whom to connect, parsed from a connection URL.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Extra query parameters from the URL, sent as settings with every
    /// statement (e.g. `?max_threads=4`).
    pub settings: Vec<(String, String)>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("secure", &self.secure)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .field("settings", &self.settings)
            .finish()
    }
}

pub(crate) const DEFAULT_HTTP_PORT: u16 = 8123;
pub(crate) const DEFAULT_HTTPS_PORT: u16 = 8443;

impl Config {
    /// Parse `clickhouse://user:pass@host:8123/db` (HTTP),
    /// `clickhouses://...` or `?secure=true` (HTTPS, default port 8443), or
    /// a plain `http(s)://` URL (default ports 80 and 443, as for any HTTP
    /// URL). The path names the database, as does `?database=`.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let url = Url::parse(text.trim()).context("not a valid ClickHouse URL")?;
        let (mut secure, native_scheme) = match url.scheme() {
            "clickhouse" | "ch" => (false, true),
            "clickhouses" => (true, true),
            "http" => (false, false),
            "https" => (true, false),
            other => bail!(
                "unsupported scheme `{other}://`; use clickhouse://, clickhouses://, http:// or https://"
            ),
        };
        let host = match url.host() {
            Some(url::Host::Domain(name)) => name.to_owned(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
            None => bail!("the URL has no host"),
        };

        let mut database = None;
        let mut settings = Vec::new();
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "secure" | "ssl" | "tls" => secure = parse_bool(&value)?,
                "database" | "db" => database = Some(value.into_owned()),
                // Credentials never travel as settings.
                "user" | "password" => bail!("put the {key} in the URL's user info, not ?{key}="),
                _ => settings.push((key.into_owned(), value.into_owned())),
            }
        }
        let path_database = url
            .path_segments()
            .and_then(|mut segments| segments.find(|s| !s.is_empty()))
            .map(decode)
            .transpose()?;
        let database = database
            .or(path_database)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "default".to_owned());

        let port = match url.port() {
            Some(port) => port,
            None if native_scheme || url.scheme() != if secure { "https" } else { "http" } => {
                if secure {
                    DEFAULT_HTTPS_PORT
                } else {
                    DEFAULT_HTTP_PORT
                }
            }
            None => url.port_or_known_default().unwrap_or(DEFAULT_HTTP_PORT),
        };
        let user = match url.username() {
            "" => "default".to_owned(),
            name => decode(name)?,
        };
        let password = url.password().map(decode).transpose()?.unwrap_or_default();

        Ok(Self {
            secure,
            host,
            port,
            user,
            password,
            database,
            settings,
        })
    }

    /// The server's HTTP endpoint, e.g. `https://host:8443/`.
    pub fn endpoint(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}:{}/", self.host, self.port)
    }

    /// `user@host:port/database`, without the password.
    pub fn display(&self) -> String {
        format!(
            "{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }
}

fn decode(text: &str) -> anyhow::Result<String> {
    Ok(percent_encoding::percent_decode_str(text)
        .decode_utf8()
        .context("the URL is not UTF-8 once decoded")?
        .into_owned())
}

fn parse_bool(value: &str) -> anyhow::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => bail!("expected true or false, got `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_urls() {
        let c = Config::parse("clickhouse://alice:s%3Acret@db.example.com:9123/sales").unwrap();
        assert!(!c.secure);
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, 9123);
        assert_eq!(c.user, "alice");
        assert_eq!(c.password, "s:cret");
        assert_eq!(c.database, "sales");
        assert_eq!(c.endpoint(), "http://db.example.com:9123/");
        assert_eq!(c.display(), "alice@db.example.com:9123/sales");
    }

    #[test]
    fn defaults_user_database_and_ports() {
        let c = Config::parse("clickhouse://localhost").unwrap();
        assert_eq!(
            (c.user.as_str(), c.port, c.database.as_str()),
            ("default", 8123, "default")
        );
        assert_eq!(c.password, "");
        assert_eq!(c.display(), "default@localhost:8123/default");

        let c = Config::parse("clickhouses://u:p@host/").unwrap();
        assert!(c.secure);
        assert_eq!(c.port, 8443);
        assert_eq!(c.endpoint(), "https://host:8443/");

        let c = Config::parse("clickhouse://host?secure=true&database=logs").unwrap();
        assert!(c.secure);
        assert_eq!(c.port, 8443);
        assert_eq!(c.database, "logs");
    }

    #[test]
    fn parses_plain_http_urls() {
        let c = Config::parse("http://localhost:8123/analytics").unwrap();
        assert!(!c.secure);
        assert_eq!((c.port, c.database.as_str()), (8123, "analytics"));

        let c = Config::parse("https://abc.clickhouse.cloud:8443").unwrap();
        assert!(c.secure);
        assert_eq!(c.port, 8443);

        // A plain HTTPS URL without a port means the HTTPS default, as it
        // would anywhere else (a proxy in front of ClickHouse).
        let c = Config::parse("https://ch.example.com/").unwrap();
        assert_eq!(c.port, 443);
        // But asking for TLS on an http:// URL uses ClickHouse's port.
        let c = Config::parse("http://ch.example.com/?secure=1").unwrap();
        assert!(c.secure);
        assert_eq!(c.port, 8443);
    }

    #[test]
    fn passes_other_parameters_as_settings() {
        let c = Config::parse("clickhouse://h/db?max_threads=4&secure=false").unwrap();
        assert_eq!(c.settings, [("max_threads".to_owned(), "4".to_owned())]);
        assert!(Config::parse("clickhouse://h/db?password=x").is_err());
    }

    #[test]
    fn rejects_other_schemes() {
        let err = Config::parse("postgres://h/db").unwrap_err().to_string();
        assert!(err.contains("postgres://"), "{err}");
        assert!(Config::parse("not a url").is_err());
    }

    #[test]
    fn never_shows_the_password() {
        let c = Config::parse("clickhouse://u:hunter2@h:1/d").unwrap();
        assert!(!c.display().contains("hunter2"));
        assert!(!format!("{c:?}").contains("hunter2"));
        assert!(!c.endpoint().contains("hunter2"));
    }

    #[test]
    fn handles_ipv6_hosts() {
        let c = Config::parse("clickhouse://[::1]:8123").unwrap();
        assert_eq!(c.endpoint(), "http://[::1]:8123/");
    }
}
