use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use http::uri::Authority;
use indexmap::IndexMap;
use serde::Deserialize;

/// Top-level configuration, loaded from `~/.config/rpanel.yaml` by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub panel: PanelConfig,
    #[serde(default)]
    pub groups: IndexMap<String, Group>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// File to append logs to. Logs go to stdout when unset.
    pub log: Option<PathBuf>,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Serve plain HTTP instead of HTTPS.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Accept invalid upstream TLS certificates (self-signed, wrong hostname).
    #[serde(default)]
    pub allow_insecure_upstream: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            log: None,
            host: default_host(),
            port: default_port(),
            allow_insecure: false,
            allow_insecure_upstream: false,
            cert: None,
            key: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    /// Reload the panel every N seconds. `0` disables automatic refresh.
    #[serde(default = "default_refresh")]
    pub refresh_seconds: u64,
    #[serde(default)]
    pub status: StatusConfig,
    /// Display name -> URL, rendered in configuration order.
    #[serde(default)]
    pub links: IndexMap<String, String>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            enable: true,
            refresh_seconds: default_refresh(),
            status: StatusConfig::default(),
            links: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    #[serde(default = "default_true")]
    pub storage: bool,
    #[serde(default = "default_true")]
    pub ram: bool,
    #[serde(default = "default_true")]
    pub uptime: bool,
    #[serde(default = "default_true")]
    pub network: bool,
    /// systemd units to report. A plain string is a system unit; use
    /// `{ name: ..., user: true }` for a user unit.
    #[serde(default)]
    pub status: Vec<Service>,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            storage: true,
            ram: true,
            uptime: true,
            network: true,
            status: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Service {
    System(String),
    Detailed {
        name: String,
        #[serde(default)]
        user: bool,
    },
}

impl Service {
    pub fn name(&self) -> &str {
        match self {
            Self::System(name) | Self::Detailed { name, .. } => name,
        }
    }

    pub fn user(&self) -> bool {
        matches!(self, Self::Detailed { user: true, .. })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Upstream authority, e.g. `127.0.0.1:8080` or `cow.local:3001`.
    pub host: String,
    #[serde(default)]
    pub https: bool,
    /// Incoming path pattern -> upstream path pattern.
    #[serde(default)]
    pub paths: IndexMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = serde_yaml_ng::from_str(text).context("invalid YAML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.server.host.trim().is_empty() {
            bail!("server.host must not be empty");
        }
        if self.server.host.chars().any(char::is_whitespace) {
            bail!("server.host must not contain whitespace");
        }
        if self.server.port == 0 {
            bail!("server.port must not be zero");
        }
        if !self.server.allow_insecure && (self.server.cert.is_none() || self.server.key.is_none())
        {
            bail!("server.cert and server.key are required unless server.allow_insecure is true");
        }

        let mut seen = HashSet::new();
        for (name, group) in &self.groups {
            validate_host(name, &group.host)?;
            if group.paths.is_empty() {
                bail!("group {name:?}: paths must not be empty");
            }
            for (from, to) in &group.paths {
                validate_pattern(name, from, "path")?;
                validate_pattern(name, to, "upstream path")?;
                if from.matches('*').count() != to.matches('*').count() {
                    bail!(
                        "group {name:?}: path {from:?} and upstream {to:?} must both contain a wildcard or neither"
                    );
                }
                if !seen.insert(from.clone()) {
                    bail!("duplicate path {from:?} across groups");
                }
            }
        }
        Ok(())
    }
}

fn validate_host(group: &str, host: &str) -> Result<()> {
    if host.trim().is_empty() {
        bail!("group {group:?}: host must not be empty");
    }
    if host.contains("://") {
        bail!("group {group:?}: host must not include a scheme");
    }
    if host.contains('/') || host.contains('@') || host.chars().any(char::is_whitespace) {
        bail!("group {group:?}: invalid host {host:?}");
    }
    Authority::from_str(host).with_context(|| format!("group {group:?}: invalid host {host:?}"))?;
    Ok(())
}

fn validate_pattern(group: &str, pattern: &str, what: &str) -> Result<()> {
    if !pattern.starts_with('/') {
        bail!("group {group:?}: {what} {pattern:?} must start with '/'");
    }
    if !pattern.is_ascii() {
        bail!(
            "group {group:?}: {what} {pattern:?} must be ASCII (percent-encode other characters)"
        );
    }
    if pattern.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("group {group:?}: {what} {pattern:?} contains whitespace or a control character");
    }
    if pattern.contains('?') || pattern.contains('#') {
        bail!("group {group:?}: {what} {pattern:?} must not contain a query or fragment");
    }
    match pattern.matches('*').count() {
        0 => {}
        1 if pattern.ends_with("/*") => {}
        _ => {
            bail!("group {group:?}: {what} {pattern:?} may only contain a single trailing wildcard")
        }
    }
    Ok(())
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    80
}

fn default_refresh() -> u64 {
    10
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server:
  log: /tmp/rpanel.log
  host: 127.0.0.1
  port: 8080
  allow_insecure: false
  cert: /tmp/cert.pem
  key: /tmp/key.pem
panel:
  enable: true
  links:
    Tary: /tary
    Name: /name
  status:
    storage: false
    ram: true
    uptime: true
    network: false
    status:
      - tary
      - name: my-unit
        user: true
groups:
  Name:
    host: 127.0.0.1:8080
    paths:
      "/": /name
      "/api/*": /nameapi/v1/*
  Tary:
    host: cow.local:3001
    paths:
      "/*": /tary/*
"#;

    #[test]
    fn parses_sample_config() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.server.port, 8080);
        assert!(!config.server.allow_insecure);
        assert_eq!(
            config.server.log.as_deref(),
            Some(Path::new("/tmp/rpanel.log"))
        );
        assert_eq!(
            config.server.cert.as_deref(),
            Some(Path::new("/tmp/cert.pem"))
        );
        assert_eq!(config.panel.links["Tary"], "/tary");
        assert!(!config.panel.status.storage);
        assert_eq!(
            config.panel.status.status,
            vec![
                Service::System("tary".into()),
                Service::Detailed {
                    name: "my-unit".into(),
                    user: true
                }
            ]
        );
        assert_eq!(config.groups["Name"].paths["/api/*"], "/nameapi/v1/*");
    }

    #[test]
    fn applies_defaults() {
        let config = Config::parse("server: { allow_insecure: true }\ngroups: {}\n").unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 80);
        assert!(config.panel.enable);
        assert_eq!(config.panel.refresh_seconds, 10);
        assert!(config.panel.status.ram);
    }

    #[test]
    fn rejects_missing_tls_material() {
        let err = Config::parse("groups: {}\n").unwrap_err().to_string();
        assert!(err.contains("server.cert"), "{err}");
    }

    #[test]
    fn rejects_bad_patterns() {
        for pattern in ["api", "/api*", "/a/*/b", "/a/**"] {
            let yaml = format!(
                "server: {{ allow_insecure: true }}\ngroups:\n  g:\n    host: localhost:1\n    paths:\n      \"{pattern}\": /x\n"
            );
            assert!(Config::parse(&yaml).is_err(), "{pattern} should fail");
        }
    }

    #[test]
    fn rejects_wildcard_mismatch() {
        let yaml = r#"
server: { allow_insecure: true }
groups:
  g:
    host: localhost:1
    paths:
      "/api/*": /x
"#;
        let err = Config::parse(yaml).unwrap_err().to_string();
        assert!(err.contains("wildcard"), "{err}");
    }

    #[test]
    fn rejects_duplicate_paths() {
        let yaml = r#"
server: { allow_insecure: true }
groups:
  a:
    host: localhost:1
    paths: { "/x": /a }
  b:
    host: localhost:2
    paths: { "/x": /b }
"#;
        let err = Config::parse(yaml).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_whitespace_in_host() {
        let yaml = r#"
server: { allow_insecure: true, host: " 127.0.0.1 " }
groups: {}
"#;
        let err = Config::parse(yaml).unwrap_err().to_string();
        assert!(err.contains("whitespace"), "{err}");
    }

    #[test]
    fn rejects_scheme_in_host() {
        let yaml = r#"
server: { allow_insecure: true }
groups:
  a:
    host: "http://localhost:1"
    paths: { "/x": /a }
"#;
        assert!(Config::parse(yaml).is_err());
    }
}
