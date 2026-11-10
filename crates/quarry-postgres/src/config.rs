//! Connection strings and TLS.
//!
//! tokio-postgres parses `postgres://` URLs and libpq `key=value` strings
//! but knows only the `disable`, `prefer` and `require` TLS modes. The
//! certificate-checking modes and `sslrootcert` are taken out here first,
//! and the rest is handed to tokio-postgres unchanged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::{Config, Host};
use tokio_postgres_rustls::MakeRustlsConnect;

/// How long to wait for a server before giving up, unless the connection
/// string says otherwise with `connect_timeout`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// libpq's `sslmode`, minus `allow`, which tries plaintext first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    /// Encrypt when the server offers TLS, without checking its certificate.
    Prefer,
    /// Always encrypt, without checking the certificate (as libpq does).
    Require,
    /// Check that the certificate chains to a trusted root.
    VerifyCa,
    /// Also check that it names the host.
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "disable" => Self::Disable,
            // `allow` would try plaintext first; prefer is the closest safe
            // reading.
            "allow" | "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            other => bail!(
                "unknown sslmode `{other}`; use disable, prefer, require, verify-ca or verify-full"
            ),
        })
    }
}

/// A parsed connection string.
pub struct ConnectOptions {
    pub config: Config,
    pub ssl_mode: SslMode,
    /// `sslrootcert`: a PEM file of trusted roots, or `system`.
    pub root_cert: Option<String>,
}

impl ConnectOptions {
    pub fn connect_timeout(&self) -> Duration {
        let per_host = self
            .config
            .get_connect_timeout()
            .copied()
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        // tokio-postgres applies the timeout per host; bound the whole
        // attempt, TLS and authentication included, by the same budget.
        let hosts = self.config.get_hosts().len().max(1) as u32;
        per_host * hosts
    }
}

/// Settings handled here rather than by tokio-postgres.
const SSL_MODE: &str = "sslmode";
const ROOT_CERT: &str = "sslrootcert";
/// Client certificates are not supported yet; refuse them rather than
/// connect without one and fail later with a less helpful message.
const UNSUPPORTED: &[&str] = &["sslcert", "sslkey", "sslpassword", "sslcrl", "sslcrldir"];

/// Parse a `postgres://` / `postgresql://` URL or a libpq `key=value`
/// string.
pub fn parse(text: &str) -> anyhow::Result<ConnectOptions> {
    let text = text.trim();
    let mut ssl_mode = None;
    let mut root_cert = None;
    let mut take = |key: &str, value: &str| -> anyhow::Result<bool> {
        match key {
            SSL_MODE => ssl_mode = Some(SslMode::parse(value)?),
            ROOT_CERT => root_cert = Some(value.to_owned()),
            key if UNSUPPORTED.contains(&key) => {
                bail!("{key} is not supported yet (client certificates)")
            }
            _ => return Ok(false),
        }
        Ok(true)
    };

    let rest = if is_url(text) {
        let (base, query) = text.split_once('?').unwrap_or((text, ""));
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if !take(&percent_decode(key), &percent_decode(value))? {
                kept.push(pair);
            }
        }
        if kept.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        }
    } else {
        let mut kept = Vec::new();
        for (key, value) in parse_key_values(text)? {
            if !take(&key, &value)? {
                kept.push(format!("{key}={}", quote_value(&value)));
            }
        }
        kept.join(" ")
    };

    let mut config: Config = rest
        .parse()
        .map_err(|err| anyhow!("invalid connection string: {err}"))?;
    if config.get_hosts().is_empty() && config.get_hostaddrs().is_empty() {
        default_host(&mut config);
    }
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
    }
    if config.get_application_name().is_none() {
        config.application_name("quarry");
    }

    let mut ssl_mode = ssl_mode.unwrap_or(SslMode::Prefer);
    // libpq checks the chain when a root certificate is given to `require`.
    if ssl_mode == SslMode::Require && root_cert.as_deref().is_some_and(|r| r != "system") {
        ssl_mode = SslMode::VerifyCa;
    }
    config.ssl_mode(match ssl_mode {
        SslMode::Disable => tokio_postgres::config::SslMode::Disable,
        SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            tokio_postgres::config::SslMode::Require
        }
    });
    Ok(ConnectOptions {
        config,
        ssl_mode,
        root_cert,
    })
}

fn is_url(text: &str) -> bool {
    text.starts_with("postgres://") || text.starts_with("postgresql://")
}

/// Like libpq: the local socket directory when one has a server, else
/// localhost over TCP.
fn default_host(config: &mut Config) {
    #[cfg(unix)]
    {
        let port = config.get_ports().first().copied().unwrap_or(5432);
        for dir in ["/tmp", "/var/run/postgresql"] {
            let socket = std::path::Path::new(dir).join(format!(".s.PGSQL.{port}"));
            if socket.exists() {
                config.host_path(dir);
                return;
            }
        }
    }
    config.host("localhost");
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a libpq `key = value` string. Values may be single-quoted, and a
/// backslash escapes the next character, quoted or not.
fn parse_key_values(text: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(pairs);
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next() != Some('=') {
            // Keys are not echoed: a malformed value may be part of a password.
            bail!("invalid connection string: expected `key=value` pairs");
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\\') => value.extend(chars.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => bail!("invalid connection string: unterminated quote"),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        pairs.push((key, value));
    }
}

fn quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `user@host:port/dbname`, never the password.
pub fn display_target(config: &Config) -> String {
    let user = config
        .get_user()
        .map(str::to_owned)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default();
    let ports = config.get_ports();
    let port = |i: usize| match ports {
        [] => 5432,
        [one] => *one,
        many => many.get(i).copied().unwrap_or(5432),
    };
    let mut hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| match host {
            Host::Tcp(name) if name.contains(':') => format!("[{name}]:{}", port(i)),
            Host::Tcp(name) => format!("{name}:{}", port(i)),
            #[cfg(unix)]
            Host::Unix(path) => format!("{}:{}", path.display(), port(i)),
        })
        .collect();
    if hosts.is_empty() {
        hosts = config
            .get_hostaddrs()
            .iter()
            .enumerate()
            .map(|(i, addr)| match addr {
                std::net::IpAddr::V6(v6) => format!("[{v6}]:{}", port(i)),
                std::net::IpAddr::V4(v4) => format!("{v4}:{}", port(i)),
            })
            .collect();
    }
    let dbname = config.get_dbname().unwrap_or(&user);
    let at = if user.is_empty() { "" } else { "@" };
    format!("{user}{at}{}/{dbname}", hosts.join(","))
}

/// The TLS connector for a connection's `sslmode`.
pub fn tls_connector(options: &ConnectOptions) -> anyhow::Result<MakeRustlsConnect> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("setting up TLS")?;
    let verifier: Arc<dyn ServerCertVerifier> = match options.ssl_mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => Arc::new(AcceptAnyCertificate {
            algorithms: provider.signature_verification_algorithms,
        }),
        SslMode::VerifyCa | SslMode::VerifyFull => {
            let roots = root_store(options.root_cert.as_deref())?;
            let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .context("setting up certificate verification")?;
            if options.ssl_mode == SslMode::VerifyCa {
                Arc::new(SkipHostname(webpki))
            } else {
                webpki
            }
        }
    };
    let config = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

fn root_store(root_cert: Option<&str>) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match root_cert {
        Some(path) if path != "system" => {
            let certs = CertificateDer::pem_file_iter(path)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .with_context(|| format!("reading sslrootcert {path}"))?;
            let (added, _) = roots.add_parsable_certificates(certs);
            if added == 0 {
                bail!("sslrootcert {path} holds no usable certificate");
            }
        }
        _ => {
            let native = rustls_native_certs::load_native_certs();
            for err in &native.errors {
                log::debug!("skipping a system root certificate: {err}");
            }
            roots.add_parsable_certificates(native.certs);
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }
    Ok(roots)
}

/// `require` and `prefer`: encrypt, but trust any certificate, like libpq.
/// The handshake signature is still checked, so the server must hold the
/// key for the certificate it sent.
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// `verify-ca`: the chain must lead to a trusted root, but the certificate
/// need not name the host.
#[derive(Debug)]
struct SkipHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for SkipHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            // webpki checks the name only after the chain is trusted.
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_targets_without_passwords() {
        let options =
            parse("postgres://alice:s3cr%40t@db.example.com:5433/app?sslmode=require").unwrap();
        let target = display_target(&options.config);
        assert_eq!(target, "alice@db.example.com:5433/app");
        assert!(!target.contains("s3cr"));
        assert_eq!(options.config.get_password(), Some(&b"s3cr@t"[..]));
        assert_eq!(options.ssl_mode, SslMode::Require);

        // libpq does not double quotes; the error must not echo the value.
        let Err(err) = parse("host=db.example.com user=bob password='it''s secret' dbname=shop")
        else {
            panic!("doubled quotes should not parse")
        };
        assert!(!err.to_string().contains("secret"), "{err}");

        let options =
            parse(r"host=db.example.com user=bob password='it\'s a secret' dbname=shop").unwrap();
        assert_eq!(
            display_target(&options.config),
            "bob@db.example.com:5432/shop"
        );
        assert_eq!(options.config.get_password(), Some(&b"it's a secret"[..]));
        assert_eq!(options.ssl_mode, SslMode::Prefer);
    }

    #[test]
    fn shows_several_hosts_and_ipv6() {
        let options = parse("postgresql://u@[::1]:5432,db2:6432/x").unwrap();
        assert_eq!(display_target(&options.config), "u@[::1]:5432,db2:6432/x");
        let options = parse("postgresql://u@h1,h2/x").unwrap();
        assert_eq!(display_target(&options.config), "u@h1:5432,h2:5432/x");
        // The database defaults to the user, as on the server.
        let options = parse("postgres://carol@h").unwrap();
        assert_eq!(display_target(&options.config), "carol@h:5432/carol");
    }

    #[test]
    fn takes_out_the_tls_settings_tokio_postgres_lacks() {
        let options =
            parse("postgres://u@h/db?sslmode=verify-full&application_name=x&sslrootcert=system")
                .unwrap();
        assert_eq!(options.ssl_mode, SslMode::VerifyFull);
        assert_eq!(options.root_cert.as_deref(), Some("system"));
        assert_eq!(options.config.get_application_name(), Some("x"));
        assert_eq!(
            options.config.get_ssl_mode(),
            tokio_postgres::config::SslMode::Require
        );

        let options = parse("host=h user=u sslmode=verify-ca connect_timeout=3").unwrap();
        assert_eq!(options.ssl_mode, SslMode::VerifyCa);
        assert_eq!(options.connect_timeout(), Duration::from_secs(3));

        // A root certificate turns `require` into `verify-ca`, as in libpq.
        let options = parse("host=h user=u sslmode=require sslrootcert=/tmp/ca.pem").unwrap();
        assert_eq!(options.ssl_mode, SslMode::VerifyCa);

        let options = parse("postgres://u@h/db?sslmode=disable").unwrap();
        assert_eq!(options.ssl_mode, SslMode::Disable);
        assert_eq!(options.connect_timeout(), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(options.config.get_application_name(), Some("quarry"));

        assert!(parse("postgres://u@h/db?sslmode=bogus").is_err());
        assert!(parse("host=h sslcert=/tmp/c.pem").is_err());
        assert!(parse("host=h nonsense=1").is_err());
    }

    #[test]
    fn builds_a_connector_for_every_mode() {
        for mode in ["disable", "prefer", "require", "verify-ca", "verify-full"] {
            let options = parse(&format!("host=h user=u sslmode={mode}")).unwrap();
            tls_connector(&options).unwrap();
        }
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
