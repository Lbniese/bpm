//! Deterministic npm configuration loading.
//!
//! BPM reads the user's `$HOME/.npmrc` first and the project's `.npmrc`
//! second. Later values override earlier values, matching npm's project-over-
//! user precedence. Only networking settings BPM consumes are interpreted;
//! unrelated npm settings are ignored for compatibility.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
pub const DEFAULT_FETCH_RETRIES: usize = 2;
pub const DEFAULT_FETCH_RETRY_FACTOR: u32 = 10;
pub const DEFAULT_FETCH_RETRY_MIN_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_FETCH_RETRY_MAX_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_FETCH_TIMEOUT_MS: u64 = 300_000;

/// Bounded retry and request timeout settings consumed by the HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    pub retries: usize,
    pub retry_factor: u32,
    pub retry_min_timeout: Duration,
    pub retry_max_timeout: Duration,
    pub fetch_timeout: Duration,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            retries: DEFAULT_FETCH_RETRIES,
            retry_factor: DEFAULT_FETCH_RETRY_FACTOR,
            retry_min_timeout: Duration::from_millis(DEFAULT_FETCH_RETRY_MIN_TIMEOUT_MS),
            retry_max_timeout: Duration::from_millis(DEFAULT_FETCH_RETRY_MAX_TIMEOUT_MS),
            fetch_timeout: Duration::from_millis(DEFAULT_FETCH_TIMEOUT_MS),
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AuthScope {
    authority: String,
    path_prefix: String,
}

/// Effective npm network configuration.
///
/// Authentication tokens are intentionally private and omitted from `Debug`.
#[derive(Clone)]
pub struct NpmConfig {
    registry: String,
    scoped_registries: BTreeMap<String, String>,
    auth_tokens: BTreeMap<AuthScope, String>,
    pub network: NetworkConfig,
}

impl Default for NpmConfig {
    fn default() -> Self {
        Self {
            registry: DEFAULT_REGISTRY.to_string(),
            scoped_registries: BTreeMap::new(),
            auth_tokens: BTreeMap::new(),
            network: NetworkConfig::default(),
        }
    }
}

impl fmt::Debug for NpmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NpmConfig")
            .field("registry", &self.registry)
            .field("scoped_registries", &self.scoped_registries)
            .field("auth_token_count", &self.auth_tokens.len())
            .field("network", &self.network)
            .finish()
    }
}

impl NpmConfig {
    /// Load `$home/.npmrc`, then `$project_dir/.npmrc`.
    ///
    /// Missing files are allowed. Other I/O and syntax failures identify the
    /// file and line without including a possibly secret value.
    pub fn load(project_dir: &Path, home: Option<&Path>) -> Result<Self, ConfigError> {
        let user = home.map(|path| path.join(".npmrc"));
        let project = project_dir.join(".npmrc");
        Self::load_paths(user.as_deref(), Some(project.as_path()))
    }

    /// Load explicit user and project npmrc paths in precedence order.
    pub fn load_paths(
        user_npmrc: Option<&Path>,
        project_npmrc: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        for path in [user_npmrc, project_npmrc].into_iter().flatten() {
            let source = match fs::read_to_string(path) {
                Ok(source) => source,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ConfigError::Read {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            };
            config.apply_source(path, &source, &|name| env::var(name).ok())?;
        }
        config.validate()?;
        Ok(config)
    }

    /// The normalized default registry URL, without a trailing slash.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// Override only the default registry after npmrc loading.
    ///
    /// The URL is validated and normalized with the same rules as an npmrc
    /// `registry` value. Scoped registries, host/path authentication, retry
    /// policy, and timeouts remain unchanged. Errors never include the
    /// supplied URL because it may contain credentials.
    pub fn with_registry_override(mut self, registry: &str) -> Result<Self, ConfigError> {
        self.registry = normalize_registry(registry).map_err(|message| {
            ConfigError::Invalid(format!("explicit registry override: {message}"))
        })?;
        Ok(self)
    }

    /// Select a scoped registry when `package` starts with `@scope/`.
    pub fn registry_for_package(&self, package: &str) -> &str {
        package
            .strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(scope, _)| self.scoped_registries.get(scope))
            .map(String::as_str)
            .unwrap_or(&self.registry)
    }

    /// Return whether an auth token is configured for this exact host/path.
    pub fn has_auth_for_url(&self, url: &str) -> bool {
        self.auth_token_for_url(url).is_some()
    }

    /// Find the most-specific host-scoped token for a request URL.
    ///
    /// This is crate-visible so the HTTP layer can construct an Authorization
    /// header while callers cannot accidentally print or serialize tokens.
    pub(crate) fn auth_token_for_url(&self, url: &str) -> Option<&str> {
        let target = parse_request_target(url)?;
        self.auth_tokens
            .iter()
            .filter(|(scope, _)| {
                scope.authority == target.authority && target.path.starts_with(&scope.path_prefix)
            })
            .max_by_key(|(scope, _)| scope.path_prefix.len())
            .map(|(_, token)| token.as_str())
    }

    fn apply_source(
        &mut self,
        path: &Path,
        source: &str,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        for (index, raw_line) in source.lines().enumerate() {
            let line_number = index + 1;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let (raw_key, raw_value) = line.split_once('=').ok_or_else(|| ConfigError::Syntax {
                path: path.to_path_buf(),
                line: line_number,
                message: "expected `key=value`".to_string(),
            })?;
            let key = raw_key.trim();
            if key.is_empty() {
                return Err(ConfigError::Syntax {
                    path: path.to_path_buf(),
                    line: line_number,
                    message: "configuration key is empty".to_string(),
                });
            }
            let value = expand_environment(raw_value.trim(), environment).map_err(|message| {
                ConfigError::Syntax {
                    path: path.to_path_buf(),
                    line: line_number,
                    message,
                }
            })?;
            self.apply_value(path, line_number, key, &value)?;
        }
        Ok(())
    }

    fn apply_value(
        &mut self,
        path: &Path,
        line: usize,
        key: &str,
        value: &str,
    ) -> Result<(), ConfigError> {
        let invalid = |message: &str| ConfigError::Syntax {
            path: path.to_path_buf(),
            line,
            message: message.to_string(),
        };

        if key.eq_ignore_ascii_case("registry") {
            self.registry = normalize_registry(value).map_err(|message| invalid(&message))?;
        } else if let Some(scope) = key
            .strip_prefix('@')
            .and_then(|key| key.strip_suffix(":registry"))
        {
            if !valid_scope(scope) {
                return Err(invalid("invalid scoped registry key"));
            }
            let registry = normalize_registry(value).map_err(|message| invalid(&message))?;
            self.scoped_registries.insert(scope.to_string(), registry);
        } else if key.starts_with("//") && key.ends_with(":_authToken") {
            if value.is_empty() {
                return Err(invalid("auth token must not be empty"));
            }
            let scope = parse_auth_scope(key).map_err(|message| invalid(&message))?;
            self.auth_tokens.insert(scope, value.to_string());
        } else if key.eq_ignore_ascii_case("fetch-retries") {
            self.network.retries =
                parse_number(value, "fetch-retries", true).map_err(|message| invalid(&message))?;
        } else if key.eq_ignore_ascii_case("fetch-retry-factor") {
            self.network.retry_factor = parse_number(value, "fetch-retry-factor", false)
                .map_err(|message| invalid(&message))?;
        } else if key.eq_ignore_ascii_case("fetch-retry-mintimeout") {
            self.network.retry_min_timeout = parse_duration(value, key).map_err(|m| invalid(&m))?;
        } else if key.eq_ignore_ascii_case("fetch-retry-maxtimeout") {
            self.network.retry_max_timeout = parse_duration(value, key).map_err(|m| invalid(&m))?;
        } else if key.eq_ignore_ascii_case("fetch-timeout") {
            self.network.fetch_timeout = parse_duration(value, key).map_err(|m| invalid(&m))?;
        }
        // npmrc contains settings for many npm commands BPM does not consume.
        // Ignore those keys rather than rejecting otherwise valid npm config.
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.network.retry_min_timeout > self.network.retry_max_timeout {
            return Err(ConfigError::Invalid(
                "fetch-retry-mintimeout must not exceed fetch-retry-maxtimeout".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Syntax {
        path: PathBuf,
        line: usize,
        message: String,
    },
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read npm config {}: {source}", path.display())
            }
            Self::Syntax {
                path,
                line,
                message,
            } => {
                write!(f, "invalid npm config {}:{line}: {message}", path.display())
            }
            Self::Invalid(message) => write!(f, "invalid merged npm config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Syntax { .. } | Self::Invalid(_) => None,
        }
    }
}

fn normalize_registry(value: &str) -> Result<String, String> {
    let target = parse_http_target(value).ok_or_else(|| {
        "registry must be an absolute http:// or https:// URL without credentials".to_string()
    })?;
    if value.contains(['?', '#']) {
        return Err("registry URL must not contain a query or fragment".to_string());
    }
    let scheme_end = value.find("://").expect("validated URL has scheme");
    let path = target.path.trim_end_matches('/');
    Ok(format!(
        "{}://{}{}",
        value[..scheme_end].to_ascii_lowercase(),
        target.authority,
        path
    ))
}

fn parse_auth_scope(key: &str) -> Result<AuthScope, String> {
    let raw = key
        .strip_prefix("//")
        .and_then(|key| key.strip_suffix(":_authToken"))
        .ok_or_else(|| "invalid auth token key".to_string())?;
    let (authority, path) = raw
        .split_once('/')
        .ok_or_else(|| "auth token key must include a host and `/` path".to_string())?;
    if authority.is_empty() || authority.contains('@') || path.contains(['?', '#']) {
        return Err("invalid host-scoped auth token key".to_string());
    }
    let path_prefix = format!("/{}/", path.trim_matches('/'));
    Ok(AuthScope {
        authority: authority.to_ascii_lowercase(),
        path_prefix: if path_prefix == "//" {
            "/".to_string()
        } else {
            path_prefix
        },
    })
}

struct RequestTarget {
    authority: String,
    path: String,
}

fn parse_request_target(value: &str) -> Option<RequestTarget> {
    parse_http_target(value)
}

fn parse_http_target(value: &str) -> Option<RequestTarget> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    let remainder = &rest[authority_end..];
    let path_end = remainder.find(['?', '#']).unwrap_or(remainder.len());
    let path = &remainder[..path_end];
    Some(RequestTarget {
        authority: authority.to_ascii_lowercase(),
        path: if path.is_empty() { "/" } else { path }.to_string(),
    })
}

fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.is_ascii()
        && scope.as_bytes()[0].is_ascii_lowercase()
        && scope.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn parse_number<T>(value: &str, key: &str, allow_zero: bool) -> Result<T, String>
where
    T: std::str::FromStr + Default + PartialEq,
{
    let parsed = value
        .parse::<T>()
        .map_err(|_| format!("{key} must be a non-negative integer"))?;
    if !allow_zero && parsed == T::default() {
        return Err(format!("{key} must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_duration(value: &str, key: &str) -> Result<Duration, String> {
    let milliseconds: u64 = parse_number(value, key, false)?;
    Ok(Duration::from_millis(milliseconds))
}

fn expand_environment(
    value: &str,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let variable = &rest[start + 2..];
        let end = variable
            .find('}')
            .ok_or_else(|| "unterminated environment variable reference".to_string())?;
        let name = &variable[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err("invalid environment variable reference".to_string());
        }
        let replacement =
            environment(name).ok_or_else(|| format!("environment variable `{name}` is not set"))?;
        expanded.push_str(&replacement);
        rest = &variable[end + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("bpm-config-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn apply(config: &mut NpmConfig, name: &str, source: &str) -> Result<(), ConfigError> {
        config.apply_source(Path::new(name), source, &|variable| {
            (variable == "BPM_TEST_TOKEN").then(|| "secret-token".to_string())
        })
    }

    #[test]
    fn project_values_override_user_values_deterministically() {
        let root = temp_dir();
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            home.join(".npmrc"),
            "registry=https://user.example/\n@team:registry=https://user-scope.example/npm/\nfetch-retries=7\n",
        )
        .unwrap();
        fs::write(
            project.join(".npmrc"),
            "registry=https://project.example/\n@team:registry=https://project-scope.example/npm/\nfetch-retries=3\n",
        )
        .unwrap();

        let config = NpmConfig::load(&project, Some(&home)).unwrap();
        assert_eq!(config.registry(), "https://project.example");
        assert_eq!(
            config.registry_for_package("@team/tool"),
            "https://project-scope.example/npm"
        );
        assert_eq!(
            config.registry_for_package("plain"),
            "https://project.example"
        );
        assert_eq!(config.network.retries, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn auth_is_host_and_path_scoped_with_longest_match() {
        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "auth.npmrc",
            "//registry.example/:_authToken=root\n//registry.example/private/:_authToken=${BPM_TEST_TOKEN}\n",
        )
        .unwrap();

        assert_eq!(
            config.auth_token_for_url("https://registry.example/private/pkg"),
            Some("secret-token")
        );
        assert_eq!(
            config.auth_token_for_url("https://registry.example/public/pkg"),
            Some("root")
        );
        assert_eq!(
            config.auth_token_for_url("https://other.example/private/pkg"),
            None
        );
    }

    #[test]
    fn parses_timeout_and_retry_settings() {
        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "network.npmrc",
            "fetch-retries=0\nfetch-retry-factor=4\nfetch-retry-mintimeout=25\nfetch-retry-maxtimeout=100\nfetch-timeout=500\n",
        )
        .unwrap();
        config.validate().unwrap();

        assert_eq!(config.network.retries, 0);
        assert_eq!(config.network.retry_factor, 4);
        assert_eq!(config.network.retry_min_timeout, Duration::from_millis(25));
        assert_eq!(config.network.retry_max_timeout, Duration::from_millis(100));
        assert_eq!(config.network.fetch_timeout, Duration::from_millis(500));
    }

    #[test]
    fn malformed_supported_values_are_actionable_without_exposing_secrets() {
        let mut config = NpmConfig::default();
        let error = apply(
            &mut config,
            "project/.npmrc",
            "//registry.example/:_authToken=${MISSING_SECRET}",
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("project/.npmrc:1"));
        assert!(message.contains("MISSING_SECRET"));
        assert!(!message.contains("secret-token"));

        let error = apply(&mut config, "project/.npmrc", "fetch-timeout=never").unwrap_err();
        assert!(error.to_string().contains("fetch-timeout"));
    }

    #[test]
    fn debug_output_redacts_tokens() {
        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "auth.npmrc",
            "//registry.example/:_authToken=highly-sensitive",
        )
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("highly-sensitive"));
        assert!(debug.contains("auth_token_count"));
    }

    #[test]
    fn rejects_unsafe_registry_and_invalid_retry_bounds() {
        let mut config = NpmConfig::default();
        assert!(apply(
            &mut config,
            "bad.npmrc",
            "registry=https://user:password@registry.example/"
        )
        .is_err());

        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "bad.npmrc",
            "fetch-retry-mintimeout=200\nfetch-retry-maxtimeout=100",
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn ignores_unrelated_valid_npm_settings_but_rejects_malformed_lines() {
        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "compatible.npmrc",
            "save-exact=true\ncolor=false",
        )
        .unwrap();
        let error = apply(&mut config, "broken.npmrc", "not-an-assignment").unwrap_err();
        assert!(error.to_string().contains("expected `key=value`"));
    }

    #[test]
    fn explicit_registry_override_is_normalized() {
        let config = NpmConfig::default()
            .with_registry_override("https://Registry.EXAMPLE/npm///")
            .unwrap();

        assert_eq!(config.registry(), "https://registry.example/npm");
    }

    #[test]
    fn invalid_explicit_registry_override_is_redaction_safe() {
        let secret = "highly-sensitive";
        let error = NpmConfig::default()
            .with_registry_override(&format!("https://user:{secret}@registry.example/"))
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("explicit registry override"));
        assert!(!message.contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn explicit_registry_override_preserves_all_loaded_settings() {
        let mut config = NpmConfig::default();
        apply(
            &mut config,
            "complete.npmrc",
            "registry=https://loaded.example/base/\n@team:registry=https://scope.example/npm/\n//loaded.example/base/:_authToken=root-token\n//scope.example/npm/:_authToken=scope-token\nfetch-retries=7\nfetch-retry-factor=3\nfetch-retry-mintimeout=125\nfetch-retry-maxtimeout=875\nfetch-timeout=4321\n",
        )
        .unwrap();
        config.validate().unwrap();
        let expected_scopes = config.scoped_registries.clone();
        let expected_tokens = config.auth_tokens.clone();
        let expected_network = config.network.clone();

        let overridden = config
            .with_registry_override("https://override.example/custom/")
            .unwrap();

        assert_eq!(overridden.registry(), "https://override.example/custom");
        assert_eq!(overridden.scoped_registries, expected_scopes);
        assert!(overridden.auth_tokens == expected_tokens);
        assert_eq!(overridden.network, expected_network);
        assert_eq!(
            overridden.registry_for_package("@team/tool"),
            "https://scope.example/npm"
        );
        assert_eq!(
            overridden.auth_token_for_url("https://loaded.example/base/pkg"),
            Some("root-token")
        );
        assert_eq!(
            overridden.auth_token_for_url("https://scope.example/npm/pkg"),
            Some("scope-token")
        );
    }
}
