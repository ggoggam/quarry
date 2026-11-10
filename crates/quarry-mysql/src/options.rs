//! Connection URLs: `mysql://user:password@host:port/database?options`.
//!
//! The `mysql` crate parses most of a URL itself, but only knows the
//! `mysql://` scheme and has no notion of the mysql client's `ssl-mode`.
//! This module accepts `mariadb://` too, takes out the options Quarry
//! handles, and hands everything else to the crate unchanged, so its own
//! options (`socket`, `compress`, `root_cert_path`, ...) still work.

use std::time::Duration;

use anyhow::{Context as _, bail};
use mysql::{Opts, OptsBuilder, SslOpts};
use percent_encoding::percent_decode_str;
use url::Url;

/// How long to wait for the server to accept a connection, unless the URL
/// says otherwise with `connect_timeout` (seconds).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The mysql client's `--ssl-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Plain TCP.
    Disabled,
    /// TLS when the server offers it, without checking its certificate;
    /// plain TCP otherwise. The mysql client's default.
    Preferred,
    /// TLS without checking the certificate; fail if the server has none.
    Required,
    /// TLS with a certificate signed by a trusted CA, for any host name.
    VerifyCa,
    /// TLS with a trusted certificate issued for this host.
    VerifyIdentity,
}

impl SslMode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(
            match value.to_ascii_lowercase().replace('-', "_").as_str() {
                "disabled" | "disable" | "false" | "off" => Self::Disabled,
                "preferred" | "prefer" => Self::Preferred,
                "required" | "require" | "true" | "on" => Self::Required,
                "verify_ca" => Self::VerifyCa,
                "verify_identity" | "verify_full" => Self::VerifyIdentity,
                other => bail!(
                    "unknown ssl-mode {other:?}; use disabled, preferred, required, verify_ca or verify_identity"
                ),
            },
        )
    }
}

/// Everything needed to open (and reopen) a connection. Holds the
/// password, so it deliberately has no `Debug`.
#[derive(Clone)]
pub struct ConnectOptions {
    /// Options for the `mysql` crate, TLS included unless disabled.
    pub opts: Opts,
    pub ssl_mode: SslMode,
    /// Display string without the password, e.g. `root@localhost:3306/shop`.
    pub target: String,
}

impl ConnectOptions {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let mut parsed = Url::parse(url).map_err(|err| anyhow::anyhow!("invalid URL: {err}"))?;
        match parsed.scheme() {
            "mysql" => {}
            // `set_scheme` refuses to swap between special and non-special
            // schemes, but neither of these is special.
            "mariadb" => parsed
                .set_scheme("mysql")
                .map_err(|()| anyhow::anyhow!("invalid URL"))?,
            other => bail!("expected a mysql:// or mariadb:// URL, got {other}://"),
        }

        let mut ssl_mode = SslMode::Preferred;
        let mut ssl_ca = None;
        let mut connect_timeout = DEFAULT_CONNECT_TIMEOUT;
        let mut prefer_socket_given = false;
        let mut passthrough = Vec::new();
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "ssl-mode" | "ssl_mode" | "sslmode" => ssl_mode = SslMode::parse(&value)?,
                "ssl-ca" | "ssl_ca" | "sslrootcert" => ssl_ca = Some(value.into_owned()),
                "connect_timeout" | "connect-timeout" => {
                    let secs: u64 = value
                        .parse()
                        .with_context(|| format!("connect_timeout {value:?} is not seconds"))?;
                    connect_timeout = Duration::from_secs(secs);
                }
                _ => {
                    prefer_socket_given |= key == "prefer_socket";
                    passthrough.push((key.into_owned(), value.into_owned()));
                }
            }
        }
        if passthrough.is_empty() {
            parsed.set_query(None);
        } else {
            parsed.query_pairs_mut().clear().extend_pairs(passthrough);
        }

        let target = display_target(&parsed);
        let opts = Opts::from_url(parsed.as_str()).map_err(|err| anyhow::anyhow!("{err}"))?;
        let mut builder = OptsBuilder::from_opts(opts).tcp_connect_timeout(Some(connect_timeout));
        if !prefer_socket_given {
            // By default the crate asks a local server for its socket path
            // and reconnects through it, which fails when the server runs
            // in a container whose socket is not on this machine.
            builder = builder.prefer_socket(false);
        }
        let ssl = match ssl_mode {
            SslMode::Disabled => None,
            SslMode::Preferred | SslMode::Required => Some(
                SslOpts::default()
                    .with_danger_accept_invalid_certs(true)
                    .with_danger_skip_domain_validation(true),
            ),
            SslMode::VerifyCa => Some(SslOpts::default().with_danger_skip_domain_validation(true)),
            SslMode::VerifyIdentity => Some(SslOpts::default()),
        };
        let ssl = match (ssl, ssl_ca) {
            (Some(ssl), Some(ca)) => {
                Some(ssl.with_root_cert_path(Some(std::path::PathBuf::from(ca))))
            }
            (ssl, _) => ssl,
        };
        // The crate's own TLS options (`root_cert_path`, ...) win when given.
        if Opts::from(builder.clone()).get_ssl_opts().is_none() {
            builder = builder.ssl_opts(ssl);
        }

        Ok(Self {
            opts: builder.into(),
            ssl_mode,
            target,
        })
    }

    /// The same options without TLS, for [`SslMode::Preferred`] when the
    /// server has no TLS.
    pub fn without_tls(&self) -> Opts {
        OptsBuilder::from_opts(self.opts.clone())
            .ssl_opts(None::<SslOpts>)
            .into()
    }
}

/// `user@host:port/database`, never the password.
fn display_target(url: &Url) -> String {
    let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
    let user = decode(url.username());
    let host = url.host_str().unwrap_or("localhost");
    let port = url.port().unwrap_or(3306);
    let database = decode(url.path().trim_start_matches('/'));
    let mut target = String::new();
    if !user.is_empty() {
        target.push_str(&user);
        target.push('@');
    }
    target.push_str(host);
    target.push(':');
    target.push_str(&port.to_string());
    if !database.is_empty() {
        target.push('/');
        target.push_str(&database);
    }
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_the_password_in_the_target() {
        let options = ConnectOptions::parse("mysql://root:s3cret@localhost:3306/shop").unwrap();
        assert_eq!(options.target, "root@localhost:3306/shop");
        assert!(!format!("{:?}", options.target).contains("s3cret"));
        assert_eq!(options.opts.get_pass(), Some("s3cret"));
        assert_eq!(options.opts.get_db_name(), Some("shop"));
    }

    #[test]
    fn accepts_mariadb_urls_and_defaults() {
        let options = ConnectOptions::parse("mariadb://app@db.internal").unwrap();
        assert_eq!(options.target, "app@db.internal:3306");
        assert_eq!(options.opts.get_tcp_port(), 3306);
        assert_eq!(options.opts.get_db_name(), None);
        assert_eq!(options.ssl_mode, SslMode::Preferred);
        assert!(!options.opts.get_prefer_socket());
        assert_eq!(
            options.opts.get_tcp_connect_timeout(),
            Some(DEFAULT_CONNECT_TIMEOUT)
        );
        assert!(options.without_tls().get_ssl_opts().is_none());
    }

    #[test]
    fn decodes_percent_encoded_parts() {
        let options =
            ConnectOptions::parse("mysql://us%40er:p%2Fw@127.0.0.1:53380/my%20db").unwrap();
        assert_eq!(options.target, "us@er@127.0.0.1:53380/my db");
        assert_eq!(options.opts.get_user(), Some("us@er"));
        assert_eq!(options.opts.get_pass(), Some("p/w"));
    }

    #[test]
    fn reads_quarry_options_and_passes_the_rest_on() {
        let options = ConnectOptions::parse(
            "mysql://root@h/db?ssl-mode=disabled&connect_timeout=3&compress=true",
        )
        .unwrap();
        assert_eq!(options.ssl_mode, SslMode::Disabled);
        assert!(options.opts.get_ssl_opts().is_none());
        assert_eq!(
            options.opts.get_tcp_connect_timeout(),
            Some(Duration::from_secs(3))
        );
        assert!(options.opts.get_compress().is_some());

        let verify = ConnectOptions::parse("mysql://root@h/db?sslmode=VERIFY_IDENTITY").unwrap();
        let ssl = verify.opts.get_ssl_opts().unwrap();
        assert!(!ssl.accept_invalid_certs());
        assert!(!ssl.skip_domain_validation());
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(ConnectOptions::parse("postgres://h/db").is_err());
        assert!(ConnectOptions::parse("mysql://h/db?ssl-mode=sometimes").is_err());
        assert!(ConnectOptions::parse("mysql://h/db?no_such_option=1").is_err());
        assert!(ConnectOptions::parse("not a url").is_err());
    }
}
