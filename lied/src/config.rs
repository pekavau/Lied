//! Layered configuration via `figment`: defaults -> optional file -> environment.
//!
//! Secrets support the Docker-secrets convention: any secret env var `X` may
//! instead be supplied as `X_FILE` (a path whose contents are read at startup)
//! or `X_CMD` (a shell command whose stdout is used). This lets operators wire
//! in Vault/1Password/pass/etc. later without changing application code.
//!
//! The `Debug` impl on [`AppConfig`] redacts every secret field so that an
//! accidental `{:?}` in logs or error messages never leaks a credential.

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

/// A secret value that redacts itself in `Debug` output.
#[derive(Clone, Serialize, Deserialize)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"[redacted]\"")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Top-level application configuration.
///
/// All fields here are **operator config** (env-owned per CLAUDE.md's
/// config-ownership tiers) — never editable via the admin UI or stored in
/// the DB. Loaded once at process startup.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// Address the HTTP server binds to.
    pub bind_addr: SocketAddr,

    /// Postgres connection string. Secret.
    pub database_url: Secret,

    /// MinIO / S3 endpoint URL, e.g. `http://localhost:9000`.
    pub s3_endpoint: String,
    /// MinIO / S3 bucket used for file storage.
    pub s3_bucket: String,
    /// MinIO / S3 access key id. Secret.
    pub s3_access_key_id: Secret,
    /// MinIO / S3 secret access key. Secret.
    pub s3_secret_access_key: Secret,
    /// MinIO / S3 region (MinIO ignores the value but the SDK requires one).
    pub s3_region: String,

    /// HS256 signing secret for bearer JWTs. Secret.
    pub jwt_signing_key: Secret,
    /// JWT lifetime in days.
    pub jwt_lifetime_days: i64,

    /// Max single file upload, in bytes.
    pub max_upload_bytes: u64,
    /// Max non-upload (JSON/form) request body, in bytes.
    pub max_request_bytes: u64,
    /// Max files per voice.
    pub max_files_per_voice: u32,
    /// Max arrangements per org (unset = no limit).
    pub max_arrangements_per_org: Option<u32>,
    /// Max list page size (offset/limit pagination ceiling).
    pub max_page_size: u32,
    /// Default list page size.
    pub default_page_size: u32,

    /// Rate limit, authenticated identity (req/min).
    pub ratelimit_auth_per_min: u32,
    /// Rate limit, unauthenticated IP (req/min).
    pub ratelimit_anon_per_min: u32,
    /// Rate limit, login/password endpoints (req/min per IP).
    pub ratelimit_login_per_min: u32,

    /// Gate for the `/metrics` endpoint (default off: unauthenticated).
    pub metrics_enabled: bool,
    /// Gate for the `/docs` RapiDoc UI (default on; `/openapi.json` is always served).
    pub docs_enabled: bool,

    /// Pretty (human-readable) tracing output instead of JSON. Typically only
    /// set in development; production defaults to JSON.
    pub tracing_pretty: bool,

    /// Whether the session cookie carries the `Secure` flag (HTTPS-only).
    /// Defaults to `false` so a fresh self-host works out of the box: browsers
    /// silently drop `Secure` cookies on non-TLS origins, so a `true` default
    /// would break login on the LAN/`localhost`/plain-HTTP deployments that
    /// self-hostability (a hard CLAUDE.md requirement, topologies 1 & 4)
    /// explicitly targets — with no error, just a redirect loop back to login.
    /// **Any deployment served over HTTPS MUST set `LIED_SECURE_COOKIES=true`**
    /// (documented in `.env.example`); the safe-by-default choice would be
    /// `true`, but a footgun that silently locks users out is worse than one
    /// that requires an opt-in flag behind TLS. `HttpOnly` and `SameSite=Lax`
    /// are not configurable — CLAUDE.md treats those as non-negotiable baseline
    /// (CSRF backstop + XSS hardening); only `Secure` depends on TLS posture.
    pub secure_cookies: bool,
}

/// Defaults mirroring the "Default limits" table in CLAUDE.md.
///
/// Internal plumbing only (figment source + extraction target) — not part
/// of the wire JSON contract, so it uses plain snake_case field names that
/// match `LIED_`-prefixed env vars directly (figment lowercases env keys).
#[derive(Serialize, Deserialize)]
struct Defaults {
    bind_addr: SocketAddr,
    s3_endpoint: String,
    s3_bucket: String,
    s3_region: String,
    jwt_lifetime_days: i64,
    max_upload_bytes: u64,
    max_request_bytes: u64,
    max_files_per_voice: u32,
    max_arrangements_per_org: Option<u32>,
    max_page_size: u32,
    default_page_size: u32,
    ratelimit_auth_per_min: u32,
    ratelimit_anon_per_min: u32,
    ratelimit_login_per_min: u32,
    metrics_enabled: bool,
    docs_enabled: bool,
    tracing_pretty: bool,
    secure_cookies: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], 8080)),
            s3_endpoint: "http://localhost:9000".to_string(),
            s3_bucket: "lied".to_string(),
            s3_region: "us-east-1".to_string(),
            jwt_lifetime_days: 30,
            max_upload_bytes: 200 * 1024 * 1024,
            max_request_bytes: 256 * 1024,
            max_files_per_voice: 50,
            max_arrangements_per_org: None,
            max_page_size: 200,
            default_page_size: 50,
            ratelimit_auth_per_min: 120,
            ratelimit_anon_per_min: 20,
            ratelimit_login_per_min: 10,
            metrics_enabled: false,
            docs_enabled: true,
            tracing_pretty: false,
            // Off by default so plain-HTTP self-hosts can log in; HTTPS
            // deployments opt in via LIED_SECURE_COOKIES=true. See the field
            // doc on `AppConfig::secure_cookies`.
            secure_cookies: false,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Figment(Box<figment::Error>),
    #[error("missing required secret: {0}")]
    MissingSecret(&'static str),
    #[error("failed to resolve secret source for {field}: {source}")]
    SecretResolution {
        field: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("secret command for {field} exited with {status:?}; stderr: {stderr}")]
    SecretCommand {
        field: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("secret {field} resolved to an empty value from {via}")]
    EmptySecret { field: &'static str, via: String },
}

impl From<figment::Error> for ConfigError {
    fn from(value: figment::Error) -> Self {
        ConfigError::Figment(Box::new(value))
    }
}

impl AppConfig {
    /// Load configuration from (in increasing precedence order):
    /// 1. Built-in defaults.
    /// 2. `config.toml` in the working directory, if present.
    /// 3. Environment variables prefixed `LIED_` (non-secret fields only).
    ///
    /// Secret fields (`database_url`, `jwt_signing_key`, `s3_access_key_id`,
    /// `s3_secret_access_key`) are deliberately *not* part of the figment
    /// layering above — they're resolved separately via [`resolve_secret`],
    /// which supports the plain env var, an `_FILE` suffix, or a `_CMD`
    /// suffix (the Docker-secrets convention). Keeping them out of the
    /// `Defaults` struct means figment never needs a (insecure) default
    /// value for them.
    pub fn load() -> Result<Self, ConfigError> {
        let figment = Figment::new()
            .merge(Serialized::defaults(Defaults::default()))
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("LIED_"));

        let defaults: Defaults = figment.extract()?;

        let database_url = resolve_secret("LIED_DATABASE_URL")?
            .map(Secret::from)
            .ok_or(ConfigError::MissingSecret("LIED_DATABASE_URL"))?;
        let s3_access_key_id = resolve_secret("LIED_S3_ACCESS_KEY_ID")?
            .map(Secret::from)
            .unwrap_or_else(|| Secret::from("minioadmin".to_string()));
        let s3_secret_access_key = resolve_secret("LIED_S3_SECRET_ACCESS_KEY")?
            .map(Secret::from)
            .unwrap_or_else(|| Secret::from("minioadmin".to_string()));
        let jwt_signing_key = resolve_secret("LIED_JWT_SIGNING_KEY")?
            .map(Secret::from)
            .ok_or(ConfigError::MissingSecret("LIED_JWT_SIGNING_KEY"))?;

        Ok(AppConfig {
            bind_addr: defaults.bind_addr,
            database_url,
            s3_endpoint: defaults.s3_endpoint,
            s3_bucket: defaults.s3_bucket,
            s3_access_key_id,
            s3_secret_access_key,
            s3_region: defaults.s3_region,
            jwt_signing_key,
            jwt_lifetime_days: defaults.jwt_lifetime_days,
            max_upload_bytes: defaults.max_upload_bytes,
            max_request_bytes: defaults.max_request_bytes,
            max_files_per_voice: defaults.max_files_per_voice,
            max_arrangements_per_org: defaults.max_arrangements_per_org,
            max_page_size: defaults.max_page_size,
            default_page_size: defaults.default_page_size,
            ratelimit_auth_per_min: defaults.ratelimit_auth_per_min,
            ratelimit_anon_per_min: defaults.ratelimit_anon_per_min,
            ratelimit_login_per_min: defaults.ratelimit_login_per_min,
            metrics_enabled: defaults.metrics_enabled,
            docs_enabled: defaults.docs_enabled,
            tracing_pretty: defaults.tracing_pretty,
            secure_cookies: defaults.secure_cookies,
        })
    }
}

/// Resolve a secret-bearing env var, supporting three sources, checked in order:
/// 1. `<NAME>` directly.
/// 2. `<NAME>_FILE` — path to a file whose trimmed contents are the secret.
/// 3. `<NAME>_CMD` — a shell command; its trimmed stdout is the secret.
///
/// A source that is *set* but yields an empty value is an error
/// ([`ConfigError::EmptySecret`]), not a silent `None` — an operator who
/// wired up a secret source clearly intended a value, so a blank result (an
/// unset vault, a truncated file) must surface loudly rather than booting the
/// app with, say, an empty JWT signing key. A non-zero exit from a `_CMD`
/// source is likewise a hard error ([`ConfigError::SecretCommand`]) rather
/// than trusting whatever landed on stdout.
///
/// Returns `Ok(None)` only when none of the three sources are set at all.
fn resolve_secret(name: &'static str) -> Result<Option<String>, ConfigError> {
    if let Ok(value) = std::env::var(name) {
        if value.is_empty() {
            return Err(ConfigError::EmptySecret {
                field: name,
                via: name.to_string(),
            });
        }
        return Ok(Some(value));
    }

    let file_var = format!("{name}_FILE");
    if let Ok(path) = std::env::var(&file_var) {
        let contents = std::fs::read_to_string(Path::new(&path)).map_err(|source| {
            ConfigError::SecretResolution {
                field: name,
                source,
            }
        })?;
        let secret = contents.trim().to_string();
        if secret.is_empty() {
            return Err(ConfigError::EmptySecret {
                field: name,
                via: file_var,
            });
        }
        return Ok(Some(secret));
    }

    let cmd_var = format!("{name}_CMD");
    if let Ok(cmd) = std::env::var(&cmd_var) {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .map_err(|source| ConfigError::SecretResolution {
                field: name,
                source,
            })?;
        if !output.status.success() {
            return Err(ConfigError::SecretCommand {
                field: name,
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if secret.is_empty() {
            return Err(ConfigError::EmptySecret {
                field: name,
                via: cmd_var,
            });
        }
        return Ok(Some(secret));
    }

    Ok(None)
}

impl fmt::Debug for AppConfig {
    /// Redacts every secret field. Non-secret operational fields are shown
    /// plainly since they aid debugging and carry no sensitive data.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("bind_addr", &self.bind_addr)
            .field("database_url", &self.database_url)
            .field("s3_endpoint", &self.s3_endpoint)
            .field("s3_bucket", &self.s3_bucket)
            .field("s3_access_key_id", &self.s3_access_key_id)
            .field("s3_secret_access_key", &self.s3_secret_access_key)
            .field("s3_region", &self.s3_region)
            .field("jwt_signing_key", &self.jwt_signing_key)
            .field("jwt_lifetime_days", &self.jwt_lifetime_days)
            .field("max_upload_bytes", &self.max_upload_bytes)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_files_per_voice", &self.max_files_per_voice)
            .field("max_arrangements_per_org", &self.max_arrangements_per_org)
            .field("max_page_size", &self.max_page_size)
            .field("default_page_size", &self.default_page_size)
            .field("ratelimit_auth_per_min", &self.ratelimit_auth_per_min)
            .field("ratelimit_anon_per_min", &self.ratelimit_anon_per_min)
            .field("ratelimit_login_per_min", &self.ratelimit_login_per_min)
            .field("metrics_enabled", &self.metrics_enabled)
            .field("docs_enabled", &self.docs_enabled)
            .field("tracing_pretty", &self.tracing_pretty)
            .field("secure_cookies", &self.secure_cookies)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secret_fields() {
        let config = AppConfig {
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            database_url: Secret::from("postgres://user:supersecret@host/db".to_string()),
            s3_endpoint: "http://localhost:9000".to_string(),
            s3_bucket: "lied".to_string(),
            s3_access_key_id: Secret::from("AKIAEXAMPLE".to_string()),
            s3_secret_access_key: Secret::from("verysecretkey".to_string()),
            s3_region: "us-east-1".to_string(),
            jwt_signing_key: Secret::from("0123456789abcdef".to_string()),
            jwt_lifetime_days: 30,
            max_upload_bytes: 1,
            max_request_bytes: 1,
            max_files_per_voice: 1,
            max_arrangements_per_org: None,
            max_page_size: 1,
            default_page_size: 1,
            ratelimit_auth_per_min: 1,
            ratelimit_anon_per_min: 1,
            ratelimit_login_per_min: 1,
            metrics_enabled: false,
            docs_enabled: true,
            tracing_pretty: false,
            secure_cookies: true,
        };

        let debug_output = format!("{config:?}");

        assert!(!debug_output.contains("supersecret"));
        assert!(!debug_output.contains("AKIAEXAMPLE"));
        assert!(!debug_output.contains("verysecretkey"));
        assert!(!debug_output.contains("0123456789abcdef"));
        assert!(debug_output.contains("[redacted]"));
        // Non-secret fields remain visible.
        assert!(debug_output.contains("lied"));
        assert!(debug_output.contains("us-east-1"));
    }

    #[test]
    fn secure_cookies_defaults_off_for_plain_http_self_hosts() {
        // Regression (#5): a `true` default silently breaks login on the
        // plain-HTTP self-host deployments self-hostability targets, because
        // browsers drop `Secure` cookies on non-TLS origins. HTTPS operators
        // opt in via LIED_SECURE_COOKIES=true.
        assert!(!Defaults::default().secure_cookies);
    }

    #[test]
    fn secret_debug_never_exposes_value() {
        let secret = Secret::from("top-secret-value".to_string());
        assert_eq!(format!("{secret:?}"), "\"[redacted]\"");
    }

    #[test]
    fn resolve_secret_reads_from_file_suffix() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lied-secret-test-{}", uuid::Uuid::now_v7()));
        std::fs::write(&path, "file-secret-value\n").unwrap();

        let file_var = "LIED_TEST_SECRET_FILE";
        std::env::set_var(file_var, &path);

        let resolved = resolve_secret("LIED_TEST_SECRET").unwrap();
        assert_eq!(resolved, Some("file-secret-value".to_string()));

        std::env::remove_var(file_var);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_secret_errors_when_command_fails() {
        // Distinct base name so the `_CMD` var can't collide with other
        // env-mutating tests running in parallel in the same binary.
        let cmd_var = "LIED_TEST_SECRET_CMDFAIL_CMD";
        std::env::set_var(cmd_var, "echo boom >&2; exit 3");

        let result = resolve_secret("LIED_TEST_SECRET_CMDFAIL");
        std::env::remove_var(cmd_var);

        match result {
            Err(ConfigError::SecretCommand { status, .. }) => assert_eq!(status, Some(3)),
            other => panic!("expected SecretCommand error, got {other:?}"),
        }
    }

    #[test]
    fn resolve_secret_errors_on_empty_command_output() {
        let cmd_var = "LIED_TEST_SECRET_CMDEMPTY_CMD";
        // Succeeds (exit 0) but produces no output.
        std::env::set_var(cmd_var, "true");

        let result = resolve_secret("LIED_TEST_SECRET_CMDEMPTY");
        std::env::remove_var(cmd_var);

        assert!(
            matches!(result, Err(ConfigError::EmptySecret { .. })),
            "expected EmptySecret error, got {result:?}"
        );
    }
}
