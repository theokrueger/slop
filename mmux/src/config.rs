use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// The canonical default configuration, also used by `mmux --print-default-config`.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../config.example.toml");

pub const DEFAULT_ROWS: u16 = 30;
pub const DEFAULT_COLS: u16 = 100;

/// Top-level mmux configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub workspace: WorkspaceConfig,
    pub terminal: TerminalConfig,
    pub new_project: NewProjectConfig,
}

impl Config {
    /// Loads the configuration from `path`, or from the default XDG location
    /// when one exists. Without a config file, mmux uses sensible defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(path) => Self::load_file(path),
            None => match Self::default_path() {
                Some(path) if path.is_file() => Self::load_file(&path),
                _ => Ok(Self::default()),
            },
        }
    }

    fn load_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("failed to load config {}", path.display()))
    }

    /// Parses and validates a TOML document.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text).context("invalid TOML")?;
        config
            .validate()
            .map_err(|message| anyhow::anyhow!(message))?;
        Ok(config)
    }

    /// The default config location: `$XDG_CONFIG_HOME/mmux/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "mmux")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(5..=500).contains(&self.terminal.rows) {
            return Err("terminal.rows must be between 5 and 500".into());
        }
        if !(10..=1000).contains(&self.terminal.cols) {
            return Err("terminal.cols must be between 10 and 1000".into());
        }
        if !(1..=24 * 365).contains(&self.auth.session_hours) {
            return Err("auth.session_hours must be between 1 and 8760".into());
        }
        if self.auth.pam_service.trim().is_empty() {
            return Err("auth.pam_service must not be empty".into());
        }
        if self.workspace.root.as_os_str().is_empty() {
            return Err("workspace.root must not be empty".into());
        }
        if self.new_project.panes.is_empty() {
            return Err("new_project.panes must contain at least one pane".into());
        }

        let mut slots = std::collections::HashSet::new();
        for (index, pane) in self.new_project.panes.iter().enumerate() {
            if !slots.insert(pane.slot) {
                return Err(format!(
                    "new_project.panes[{index}] uses slot `{}` more than once",
                    pane.slot
                ));
            }
            if let Some(program) = pane.command.first() {
                if program.is_empty() {
                    return Err(format!(
                        "new_project.panes[{index}].command must not start with an empty string"
                    ));
                }
                if pane.command.iter().any(|arg| arg.contains('\0')) {
                    return Err(format!(
                        "new_project.panes[{index}].command contains a NUL byte"
                    ));
                }
            }
            if pane
                .env
                .keys()
                .any(|key| key.is_empty() || key.contains('='))
            {
                return Err(format!(
                    "new_project.panes[{index}].env contains an invalid variable name"
                ));
            }
        }
        Ok(())
    }
}

/// Validates a user-visible project or pane label.
pub fn validate_label(label: &str) -> Result<(), String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("must not be empty".into());
    }
    if label.chars().count() > 80 {
        return Err("must be at most 80 characters".into());
    }
    if label.chars().any(char::is_control) {
        return Err("must not contain control characters".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to listen on. Keep this on loopback and use a TLS
    /// reverse proxy (or an SSH tunnel) for remote access.
    pub bind: SocketAddr,
    /// Mark the session cookie `Secure`. Enable when served over HTTPS.
    pub secure_cookies: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8787)),
            secure_cookies: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// PAM service used to verify Linux credentials. The service must allow
    /// password authentication for the user mmux runs as.
    pub pam_service: String,
    /// Session lifetime, refreshed on use.
    pub session_hours: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            pam_service: "login".into(),
            session_hours: 24,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Interactive directory picker is confined to this subtree.
    /// `~` is expanded. Set to `/` to browse the whole filesystem.
    pub root: PathBuf,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("~"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Initial pane size before a client negotiates its viewport.
    pub rows: u16,
    pub cols: u16,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NewProjectConfig {
    /// Panes started, in the selected working directory, for every new project.
    pub panes: Vec<PaneConfig>,
}

impl Default for NewProjectConfig {
    fn default() -> Self {
        Self {
            panes: default_panes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PaneConfig {
    pub slot: Slot,
    /// Program and arguments. An empty list starts `$SHELL`.
    #[serde(default)]
    pub command: Vec<String>,
    /// Extra environment variables for this pane.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// One of the four cells of the pane grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Slot {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Slot {
    pub const ALL: [Slot; 4] = [
        Slot::TopLeft,
        Slot::TopRight,
        Slot::BottomLeft,
        Slot::BottomRight,
    ];
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Slot::TopLeft => "top-left",
            Slot::TopRight => "top-right",
            Slot::BottomLeft => "bottom-left",
            Slot::BottomRight => "bottom-right",
        })
    }
}

fn default_panes() -> Vec<PaneConfig> {
    vec![
        PaneConfig {
            slot: Slot::TopLeft,
            command: vec!["pi".into(), "--session-id".into(), "{session_id}".into()],
            env: BTreeMap::new(),
        },
        PaneConfig {
            slot: Slot::BottomLeft,
            command: vec!["pi".into(), "--session-id".into(), "{session_id}".into()],
            env: BTreeMap::new(),
        },
        PaneConfig {
            slot: Slot::TopRight,
            command: vec!["emacs".into(), "-nw".into()],
            env: BTreeMap::new(),
        },
        PaneConfig {
            slot: Slot::BottomRight,
            command: Vec::new(),
            env: BTreeMap::new(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_matches_defaults() {
        let parsed = Config::from_toml(DEFAULT_CONFIG_TOML).expect("example config must parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn empty_document_uses_defaults_except_required_new_project() {
        // `new_project` has a serde default, so an empty document is valid.
        let parsed = Config::from_toml("").expect("empty config is valid");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn rejects_duplicate_slots() {
        let text = r#"
            [[new_project.panes]]
            slot = "top-left"
            [[new_project.panes]]
            slot = "top-left"
        "#;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn rejects_out_of_range_sizes() {
        let text = "[terminal]\nrows = 2\n";
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("terminal.rows"), "{err}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = format!(
            "{:#}",
            Config::from_toml("[server]\nprot = 1\n").unwrap_err()
        );
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn pane_names_are_no_longer_configurable() {
        let text = r#"
            [[new_project.panes]]
            slot = "top-left"
            name = "legacy"
        "#;
        let err = format!("{:#}", Config::from_toml(text).unwrap_err());
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn slot_round_trips_through_serde() {
        for slot in Slot::ALL {
            let json = serde_json::to_string(&slot).unwrap();
            assert_eq!(serde_json::from_str::<Slot>(&json).unwrap(), slot);
        }
    }
}
