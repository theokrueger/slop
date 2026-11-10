use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub models: BTreeMap<String, Model>,
    pub users: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Server {
    pub host: String,
    pub port: String,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub log: Option<PathBuf>,
    pub rate_limit: u32,
    pub ip_rate_limit: u32,
    pub max_concurrency: usize,
    pub allowed_paths: Vec<String>,
    pub max_body_bytes: usize,
    pub body_timeout_secs: u64,
    pub upstream_read_timeout_secs: u64,
    pub allow_insecure: bool,
    pub allow_insecure_upstream: bool,
}

fn default_allowed_paths() -> Vec<String> {
    vec![
        "/v1/chat/completions".into(),
        "/v1/completions".into(),
        "/v1/embeddings".into(),
        "/v1/responses".into(),
    ]
}

impl Default for Server {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: "8001".into(),
            cert: None,
            key: None,
            log: None,
            rate_limit: 100,
            ip_rate_limit: 300,
            max_concurrency: 128,
            allowed_paths: default_allowed_paths(),
            max_body_bytes: 16 << 20,
            body_timeout_secs: 30,
            upstream_read_timeout_secs: 300,
            allow_insecure: false,
            allow_insecure_upstream: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modelname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning: bool,
    #[serde(default = "default_input", skip_serializing_if = "is_default_input")]
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Cost::is_default")]
    pub cost: Cost,
    #[serde(
        default = "default_context_window",
        skip_serializing_if = "is_default_context_window"
    )]
    pub context_window: u64,
    #[serde(
        default = "default_max_tokens",
        skip_serializing_if = "is_default_max_tokens"
    )]
    pub max_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(alias = "cacheRead")]
    pub cache_read: f64,
    #[serde(alias = "cacheWrite")]
    pub cache_write: f64,
}

impl Cost {
    fn is_default(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_read == 0.0 && self.cache_write == 0.0
    }
}

fn default_input() -> Vec<String> {
    vec!["text".into()]
}

fn default_context_window() -> u64 {
    128_000
}

fn default_max_tokens() -> u64 {
    16_384
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_input(value: &[String]) -> bool {
    value.len() == 1 && value[0] == "text"
}

fn is_default_context_window(value: &u64) -> bool {
    *value == default_context_window()
}

fn is_default_max_tokens(value: &u64) -> bool {
    *value == default_max_tokens()
}

impl Default for Model {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            modelname: None,
            api_key: None,
            name: None,
            reasoning: false,
            input: default_input(),
            cost: Cost::default(),
            context_window: default_context_window(),
            max_tokens: default_max_tokens(),
        }
    }
}

impl Model {
    pub fn upstream_name<'a>(&'a self, alias: &'a str) -> &'a str {
        self.modelname.as_deref().unwrap_or(alias)
    }

    pub fn display_name<'a>(&'a self, alias: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(alias)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let config: Self = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.server.cert.is_some() != self.server.key.is_some() {
            bail!("server.cert and server.key must be set together");
        }
        if self.server.max_body_bytes == 0 {
            bail!("server.max_body_bytes must be greater than zero");
        }
        if self.server.body_timeout_secs == 0 {
            bail!("server.body_timeout_secs must be greater than zero");
        }
        if self.server.upstream_read_timeout_secs == 0 {
            bail!("server.upstream_read_timeout_secs must be greater than zero");
        }
        if self.server.max_concurrency == 0 {
            bail!("server.max_concurrency must be greater than zero");
        }
        if self.server.allowed_paths.is_empty() {
            bail!("server.allowed_paths cannot be empty");
        }
        for path in &self.server.allowed_paths {
            if !path.starts_with("/v1/")
                || path.contains("..")
                || path.contains(['?', '#', '\\'])
                || path.to_ascii_lowercase().contains("%2e")
                || path.chars().any(char::is_whitespace)
            {
                bail!("invalid server.allowed_paths entry {path:?}");
            }
        }
        for (alias, model) in &self.models {
            if alias.trim().is_empty() {
                bail!("model alias cannot be empty");
            }
            let url = reqwest::Url::parse(&model.endpoint)
                .with_context(|| format!("invalid endpoint for model {alias:?}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("model {alias:?} endpoint must use http or https");
            }
            if !url.username().is_empty() || url.password().is_some() {
                bail!("model {alias:?} endpoint must not contain credentials; use api_key");
            }
            if url.scheme() == "http" && !self.server.allow_insecure_upstream {
                let host = url.host_str().unwrap_or_default();
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
                if !loopback {
                    bail!(
                        "model {alias:?} uses plaintext HTTP; use HTTPS or set server.allow_insecure_upstream: true"
                    );
                }
            }
            if model.context_window == 0 {
                bail!("model {alias:?} context_window must be greater than zero");
            }
            if model.max_tokens == 0 {
                bail!("model {alias:?} max_tokens must be greater than zero");
            }
            if model.input.is_empty()
                || model
                    .input
                    .iter()
                    .any(|input| !matches!(input.as_str(), "text" | "image"))
            {
                bail!("model {alias:?} input must contain only text or image");
            }
            if let Some(name) = &model.name
                && name.trim().is_empty()
            {
                bail!("model {alias:?} name cannot be empty");
            }
            if [
                model.cost.input,
                model.cost.output,
                model.cost.cache_read,
                model.cost.cache_write,
            ]
            .iter()
            .any(|cost| !cost.is_finite() || *cost < 0.0)
            {
                bail!("model {alias:?} cost values must be finite and non-negative");
            }
        }
        for (name, key) in &self.users {
            if name.trim().is_empty() {
                bail!("user name cannot be empty");
            }
            if key.is_empty() {
                bail!("user {name:?} has an empty API key");
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let text = serde_yaml_ng::to_string(self).context("serialize config")?;
        fs::write(path, text).with_context(|| format!("write config {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set permissions on {}", path.display()))?;
        Ok(())
    }
}

pub fn warn_if_insecure(path: &Path) {
    warn_if_accessible(path, "config file");
}

pub fn warn_if_insecure_key(path: &Path) {
    warn_if_accessible(path, "TLS private key");
}

fn warn_if_accessible(path: &Path, kind: &str) {
    if let Ok(metadata) = fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:o}"),
                kind,
                "file is accessible by other users"
            );
        }
    }
}

pub fn default_path() -> PathBuf {
    PathBuf::from("/etc/tproxy.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let config: Config = serde_yaml_ng::from_str(
            r#"
server:
  host: "0.0.0.0"
  port: "8001"
  cert: "/path/to/cert"
  key: "/path/to/key"
  log: "/path/to/log"

models:
  "morph-dsv41flash-1017":
    endpoint: "https://api.morphllm.com/v1"
    modelname: "morph-dsv41flash-1017"
    api_key: "morph-api-key"

users:
  me: "API_KEY"
"#,
        )
        .unwrap();

        assert_eq!(config.server.port, "8001");
        assert_eq!(config.server.cert.unwrap(), Path::new("/path/to/cert"));
        assert_eq!(
            config.models["morph-dsv41flash-1017"].endpoint,
            "https://api.morphllm.com/v1"
        );
        assert_eq!(config.users["me"], "API_KEY");
    }

    #[test]
    fn default_config_path_is_etc() {
        assert_eq!(default_path(), Path::new("/etc/tproxy.yaml"));
    }

    #[test]
    fn defaults_are_optional() {
        let config: Config = serde_yaml_ng::from_str("models: {}\n").unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, "8001");
        assert_eq!(config.server.rate_limit, 100);
        assert_eq!(config.server.ip_rate_limit, 300);
        assert_eq!(config.server.max_concurrency, 128);
        assert_eq!(config.server.max_body_bytes, 16 << 20);
        assert_eq!(config.server.allowed_paths, default_allowed_paths());
        assert!(config.server.cert.is_none());
        assert!(config.users.is_empty());
    }

    #[test]
    fn rejects_unsafe_paths_and_incomplete_tls() {
        let mut config = Config::default();
        config.server.cert = Some("/tmp/cert".into());
        assert!(config.validate().is_err());

        config.server.cert = None;
        config.server.allowed_paths = vec!["/v1/../admin".into()];
        assert!(config.validate().is_err());

        config.server.allowed_paths = vec!["/v1/chat/completions".into()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_remote_plaintext_upstream() {
        let mut config = Config::default();
        let model = Model {
            endpoint: "http://example.com/v1".into(),
            ..Default::default()
        };
        config.models.insert("remote".into(), model.clone());
        assert!(config.validate().is_err());
        config.server.allow_insecure_upstream = true;
        assert!(config.validate().is_ok());

        config.server.allow_insecure_upstream = false;
        config.models.clear();
        config.models.insert(
            "local".into(),
            Model {
                endpoint: "http://127.0.0.1:8080/v1".into(),
                ..model
            },
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn modelname_falls_back_to_alias() {
        let model = Model {
            endpoint: "http://localhost/v1".into(),
            ..Default::default()
        };
        assert_eq!(model.upstream_name("alias"), "alias");
        assert_eq!(model.display_name("alias"), "alias");
        assert_eq!(model.context_window, 128_000);
        assert_eq!(model.max_tokens, 16_384);
        assert_eq!(model.input, ["text"]);
    }

    #[test]
    fn parses_model_metadata() {
        let config: Config = serde_yaml_ng::from_str(
            r#"
models:
  alias:
    endpoint: "https://example.com/v1"
    name: "Alias Model"
    context_window: 64000
    max_tokens: 8000
    reasoning: true
    input: ["text", "image"]
    cost:
      input: 1.5
      output: 2.5
      cache_read: 0.5
      cache_write: 0.25
users: {}
"#,
        )
        .unwrap();
        let model = &config.models["alias"];
        assert_eq!(model.display_name("alias"), "Alias Model");
        assert_eq!(model.context_window, 64_000);
        assert_eq!(model.max_tokens, 8_000);
        assert!(model.reasoning);
        assert_eq!(model.input, ["text", "image"]);
        assert_eq!(model.cost.cache_read, 0.5);
        assert_eq!(model.cost.cache_write, 0.25);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tproxy.yaml");
        let mut config = Config::default();
        config.users.insert("me".into(), "sk-test".into());
        config.models.insert(
            "alias".into(),
            Model {
                endpoint: "http://localhost/v1".into(),
                modelname: Some("upstream".into()),
                ..Default::default()
            },
        );
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().users["me"], "sk-test");
    }

    #[test]
    fn save_sets_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tproxy.yaml");
        Config::default().save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
