use http::uri::Authority;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

const DEFAULT_IDLE_TIMEOUT_SECONDS: u64 = 1_800;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: String,
    services: Vec<RawService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    host: String,
    upstream: String,
    launchd_label: String,
    #[serde(default = "default_idle_timeout_seconds")]
    idle_timeout_seconds: u64,
    can_stop_command: Option<Vec<String>>,
}

fn default_idle_timeout_seconds() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECONDS
}

#[derive(Debug)]
pub struct Config {
    listen: SocketAddr,
    services: BTreeMap<String, ServiceConfig>,
}

#[derive(Debug)]
pub struct ServiceConfig {
    host: String,
    upstream: SocketAddr,
    launchd_label: String,
    idle_timeout_seconds: u64,
    can_stop_command: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&input)
    }

    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input).map_err(ConfigError::Parse)?;
        let listen = raw.listen.parse::<SocketAddr>().map_err(|_| {
            ConfigError::Invalid("listen must be a literal IP socket address".into())
        })?;
        if listen.port() < 1_024 {
            return Err(ConfigError::Invalid(
                "listen must use a fixed unprivileged port (1024 or greater)".into(),
            ));
        }

        if raw.services.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one service must be configured".into(),
            ));
        }

        let mut services = BTreeMap::new();
        let mut labels = HashSet::new();
        let mut upstreams = HashSet::new();

        for (index, raw_service) in raw.services.into_iter().enumerate() {
            let service_number = index + 1;
            let host = normalize_config_host(&raw_service.host).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "service {service_number} host must be an ASCII DNS hostname without a port"
                ))
            })?;

            let upstream = raw_service.upstream.parse::<SocketAddr>().map_err(|_| {
                ConfigError::Invalid(format!(
                    "service {service_number} upstream must be a literal IP socket address"
                ))
            })?;
            if !upstream.ip().is_loopback() || upstream.port() == 0 {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} upstream must be a loopback address with a nonzero port"
                )));
            }

            if !valid_launchd_label(&raw_service.launchd_label) {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} launchd_label is invalid"
                )));
            }
            if raw_service.idle_timeout_seconds == 0 {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} idle_timeout_seconds must be greater than zero"
                )));
            }
            validate_command(raw_service.can_stop_command.as_deref(), service_number)?;

            if services.contains_key(&host) {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} duplicates normalized host {host}"
                )));
            }
            if !labels.insert(raw_service.launchd_label.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} duplicates a launchd_label"
                )));
            }
            if !upstreams.insert(upstream) {
                return Err(ConfigError::Invalid(format!(
                    "service {service_number} duplicates an upstream"
                )));
            }

            services.insert(
                host.clone(),
                ServiceConfig {
                    host,
                    upstream,
                    launchd_label: raw_service.launchd_label,
                    idle_timeout_seconds: raw_service.idle_timeout_seconds,
                    can_stop_command: raw_service.can_stop_command,
                },
            );
        }

        Ok(Self { listen, services })
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    pub fn services(&self) -> impl Iterator<Item = &ServiceConfig> {
        self.services.values()
    }

    pub fn service_for_request_host(&self, host: &str) -> Option<&ServiceConfig> {
        let host = normalize_request_host(host, self.listen.port())?;
        self.services.get(&host)
    }
}

impl ServiceConfig {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn upstream(&self) -> SocketAddr {
        self.upstream
    }

    pub fn launchd_label(&self) -> &str {
        &self.launchd_label
    }

    pub fn idle_timeout_seconds(&self) -> u64 {
        self.idle_timeout_seconds
    }

    pub fn can_stop_command(&self) -> Option<&[String]> {
        self.can_stop_command.as_deref()
    }
}

fn validate_command(command: Option<&[String]>, service_number: usize) -> Result<(), ConfigError> {
    let Some(command) = command else {
        return Ok(());
    };
    let Some(executable) = command.first() else {
        return Err(ConfigError::Invalid(format!(
            "service {service_number} can_stop_command must not be empty"
        )));
    };
    if executable.is_empty() || !Path::new(executable).is_absolute() {
        return Err(ConfigError::Invalid(format!(
            "service {service_number} can_stop_command executable must be an absolute path"
        )));
    }
    Ok(())
}

fn valid_launchd_label(label: &str) -> bool {
    !label.is_empty()
        && label.chars().all(|character| {
            character != '/' && !character.is_control() && !character.is_whitespace()
        })
}

fn normalize_config_host(host: &str) -> Option<String> {
    normalize_authority(host, None)
}

pub(crate) fn normalize_request_host(host: &str, listener_port: u16) -> Option<String> {
    normalize_authority(host, Some(listener_port))
}

fn normalize_authority(authority: &str, allowed_port: Option<u16>) -> Option<String> {
    if authority.is_empty() || !authority.is_ascii() || authority.contains('@') {
        return None;
    }

    let raw_authority = authority;
    let authority = raw_authority.parse::<Authority>().ok()?;
    if raw_authority.ends_with(':') {
        return None;
    }
    let port = match authority.port() {
        Some(port) => Some(port.as_str().parse::<u16>().ok()?),
        None if raw_authority.contains(':') => return None,
        None => None,
    };
    match (port, allowed_port) {
        (Some(actual), Some(expected)) if actual == expected => {}
        (Some(_), _) => return None,
        (None, _) => {}
    }

    normalize_dns_name(authority.host())
}

fn normalize_dns_name(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 || !host.is_ascii() || host.parse::<IpAddr>().is_ok() {
        return None;
    }

    let valid = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    });
    valid.then(|| host.to_ascii_lowercase())
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    Invalid(String),
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse(source) => write!(formatter, "invalid TOML configuration: {source}"),
            Self::Invalid(message) => write!(formatter, "invalid configuration: {message}"),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
listen = "127.0.0.1:8080"

[[services]]
host = "Alpha.Apps.Test."
upstream = "127.0.0.1:9001"
launchd_label = "net.test.alpha"
"#;

    fn error(input: &str) -> String {
        Config::from_toml(input).unwrap_err().to_string()
    }

    #[test]
    fn parses_defaults_and_lifecycle_fields_without_using_them() {
        let config = Config::from_toml(VALID).unwrap();
        let service = config.services().next().unwrap();
        assert_eq!(config.listen(), "127.0.0.1:8080".parse().unwrap());
        assert_eq!(service.host(), "alpha.apps.test");
        assert_eq!(service.upstream(), "127.0.0.1:9001".parse().unwrap());
        assert_eq!(service.launchd_label(), "net.test.alpha");
        assert_eq!(service.idle_timeout_seconds(), 1_800);
        assert!(service.can_stop_command().is_none());
    }

    #[test]
    fn parses_explicit_lifecycle_fields_as_opaque_configuration() {
        let input = VALID.replace(
            "launchd_label = \"net.test.alpha\"",
            "launchd_label = \"net.test.alpha\"\nidle_timeout_seconds = 42\ncan_stop_command = [\"/opt/test/check\", \"\"]",
        );
        let config = Config::from_toml(&input).unwrap();
        let service = config.services().next().unwrap();
        assert_eq!(service.idle_timeout_seconds(), 42);
        assert_eq!(service.can_stop_command().unwrap(), ["/opt/test/check", ""]);
    }

    #[test]
    fn rejects_unknown_fields_at_both_levels() {
        assert!(
            error(&VALID.replace("listen =", "extra = true\nlisten =")).contains("unknown field")
        );
        assert!(error(&VALID.replace("host =", "extra = true\nhost =")).contains("unknown field"));
    }

    #[test]
    fn rejects_malformed_or_nonloopback_addresses() {
        assert!(error(&VALID.replace("127.0.0.1:8080", "localhost:8080")).contains("listen"));
        for listen in ["127.0.0.1:0", "127.0.0.1:80"] {
            assert!(error(&VALID.replace("127.0.0.1:8080", listen)).contains("unprivileged"));
        }
        for upstream in [
            "localhost:9001",
            "0.0.0.0:9001",
            "192.0.2.1:9001",
            "127.0.0.1:0",
        ] {
            assert!(error(&VALID.replace("127.0.0.1:9001", upstream)).contains("upstream"));
        }
        assert!(Config::from_toml(&VALID.replace("127.0.0.1:9001", "[::1]:9001")).is_ok());
    }

    #[test]
    fn rejects_every_named_duplicate() {
        let second = r#"
[[services]]
host = "beta.apps.test"
upstream = "127.0.0.1:9002"
launchd_label = "net.test.beta"
"#;
        assert!(
            error(&format!(
                "{VALID}{}",
                second.replace("beta.apps.test", "ALPHA.APPS.TEST")
            ))
            .contains("host")
        );
        assert!(
            error(&format!(
                "{VALID}{}",
                second.replace("net.test.beta", "net.test.alpha")
            ))
            .contains("launchd_label")
        );
        assert!(
            error(&format!(
                "{VALID}{}",
                second.replace("127.0.0.1:9002", "127.0.0.1:9001")
            ))
            .contains("upstream")
        );
    }

    #[test]
    fn rejects_invalid_lifecycle_values() {
        assert!(
            error(&VALID.replace(
                "launchd_label = \"net.test.alpha\"",
                "launchd_label = \"bad/label\""
            ))
            .contains("launchd_label")
        );
        assert!(
            error(&VALID.replace(
                "launchd_label = \"net.test.alpha\"",
                "launchd_label = \"bad label\""
            ))
            .contains("launchd_label")
        );
        assert!(
            error(&VALID.replace(
                "launchd_label = \"net.test.alpha\"",
                "launchd_label = \"net.test.alpha\"\nidle_timeout_seconds = 0"
            ))
            .contains("idle_timeout")
        );
        assert!(
            error(&VALID.replace(
                "launchd_label = \"net.test.alpha\"",
                "launchd_label = \"net.test.alpha\"\ncan_stop_command = []"
            ))
            .contains("must not be empty")
        );
        assert!(
            error(&VALID.replace(
                "launchd_label = \"net.test.alpha\"",
                "launchd_label = \"net.test.alpha\"\ncan_stop_command = [\"relative\"]"
            ))
            .contains("absolute path")
        );
    }

    #[test]
    fn normalizes_only_supported_request_hosts() {
        for host in [
            "ALPHA.APPS.TEST",
            "alpha.apps.test.",
            "AlPhA.Apps.Test:8080",
            "AlPhA.Apps.Test.:8080",
        ] {
            assert_eq!(
                normalize_request_host(host, 8080).as_deref(),
                Some("alpha.apps.test")
            );
        }
        for host in [
            "alpha.apps.test:8081",
            "alpha.apps.test:",
            "alpha.apps.test:99999",
            "user@alpha.apps.test",
            "alpha..apps.test",
            "127.0.0.1",
            "[::1]:8080",
            "alpha_apps.test",
            "álpha.apps.test",
        ] {
            assert!(normalize_request_host(host, 8080).is_none(), "{host}");
        }
    }

    #[test]
    fn validation_is_atomic_when_a_later_service_is_invalid() {
        let input = format!(
            "{VALID}\n[[services]]\nhost = \"beta.apps.test\"\nupstream = \"203.0.113.8:9002\"\nlaunchd_label = \"net.test.beta\"\n"
        );
        assert!(Config::from_toml(&input).is_err());
    }
}
