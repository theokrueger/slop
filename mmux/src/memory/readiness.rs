//! Read-only check that pi can see the memory MCP server.
//!
//! mmux never edits pi's configuration. It reports a verdict once at startup,
//! and the same verdict carries copy-paste remediation.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::MemoryConfig;

/// The `mcp.json` snippet users paste, kept in one place so the log, the API,
/// and the CLI cannot drift apart.
pub const ENTRY_JSON: &str = r#""mmux": {
  "command": "mmux",
  "args": ["mcp"],
  "description": "Project memory: search, read, and write durable project notes",
  "exposure": "deferred",
  "toolExposure": {
    "memory_search": "direct",
    "memory_read": "direct",
    "memory_write": "deferred"
  },
  "timeout": 20
}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemoryReadiness {
    /// The entry exists and looks runnable.
    Ready { server: String },
    /// `pi` is not installed; memory still works from the CLI and the UI.
    NotApplicable,
    /// `mcp.json` exists but has no mmux entry.
    Missing { config: PathBuf },
    /// The entry exists but is disabled with `enabled: false`.
    Disabled { config: PathBuf },
    /// The entry exists but would not work as intended.
    Mismatched { config: PathBuf, reason: String },
    /// `mcp.json` could not be read or parsed.
    Invalid { config: PathBuf, error: String },
    /// More than one entry looks like an mmux server.
    Ambiguous { config: PathBuf },
}

impl MemoryReadiness {
    pub fn is_ok(&self) -> bool {
        matches!(
            self,
            MemoryReadiness::Ready { .. } | MemoryReadiness::NotApplicable
        )
    }

    pub fn state_name(&self) -> &'static str {
        match self {
            MemoryReadiness::Ready { .. } => "ready",
            MemoryReadiness::NotApplicable => "not_applicable",
            MemoryReadiness::Missing { .. } => "missing",
            MemoryReadiness::Disabled { .. } => "disabled",
            MemoryReadiness::Mismatched { .. } => "mismatched",
            MemoryReadiness::Invalid { .. } => "invalid",
            MemoryReadiness::Ambiguous { .. } => "ambiguous",
        }
    }

    /// One line for a log or a banner.
    pub fn message(&self) -> Option<String> {
        match self {
            MemoryReadiness::Ready { .. } | MemoryReadiness::NotApplicable => None,
            MemoryReadiness::Missing { config } => Some(format!(
                "pi memory server not configured: no `mmux` entry in {}",
                config.display()
            )),
            MemoryReadiness::Disabled { config } => Some(format!(
                "pi memory server disabled: `enabled = false` in {}",
                config.display()
            )),
            MemoryReadiness::Mismatched { config, reason } => Some(format!(
                "pi memory server misconfigured in {}: {reason}",
                config.display()
            )),
            MemoryReadiness::Invalid { config, error } => Some(format!(
                "could not read pi MCP configuration {}: {error}",
                config.display()
            )),
            MemoryReadiness::Ambiguous { config } => Some(format!(
                "more than one mmux entry in {}; pi loads only one",
                config.display()
            )),
        }
    }

    /// Multi-line, copy-paste remediation. Never applied automatically.
    pub fn advice(&self) -> Option<String> {
        let config = self.config()?;
        // The most common failure is not the entry but the PATH: pi spawns the
        // server with the environment it inherited, which may not include the
        // directory `cargo install` wrote to.
        let hint = match self {
            MemoryReadiness::Mismatched { reason, .. } if reason.contains("does not resolve") => {
                "pi spawns this server with the PATH it inherited, which can differ from\n\
                 your interactive shell. Put the mmux binary on that PATH (for the systemd\n\
                 unit: %h/.cargo/bin) or use an absolute `command`.\n\n"
            }
            _ => "",
        };
        Some(format!(
            "{hint}Add this to {} under \"mcpServers\" (mmux will not do it for you):\n\n{ENTRY_JSON}\n\n\
             Then restart pi panes, or run:\n  pi mcp add mmux --exposure deferred -- mmux mcp\n\n\
             A project-level .pi/mcp.json can still override or disable it.",
            config.display()
        ))
    }

    pub fn config(&self) -> Option<&Path> {
        match self {
            MemoryReadiness::Ready { .. } | MemoryReadiness::NotApplicable => None,
            MemoryReadiness::Missing { config }
            | MemoryReadiness::Disabled { config }
            | MemoryReadiness::Mismatched { config, .. }
            | MemoryReadiness::Invalid { config, .. }
            | MemoryReadiness::Ambiguous { config } => Some(config),
        }
    }
}

/// Runs the probe. Read-only: nothing here creates, fixes, or rewrites a file.
pub fn check(config: &MemoryConfig) -> MemoryReadiness {
    if !config.enabled {
        return MemoryReadiness::NotApplicable;
    }
    if which("pi").is_none() {
        return MemoryReadiness::NotApplicable;
    }

    let dir = super::vault::expand(&config.pi_agent_dir);
    let path = dir.join("mcp.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return MemoryReadiness::Missing { config: path };
        }
        Err(err) => {
            return MemoryReadiness::Invalid {
                config: path,
                error: err.to_string(),
            };
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            return MemoryReadiness::Invalid {
                config: path,
                error: err.to_string(),
            };
        }
    };

    let Some(servers) = value.get("mcpServers").and_then(|value| value.as_object()) else {
        return MemoryReadiness::Missing { config: path };
    };

    let mut matches: Vec<(&String, &serde_json::Value)> = servers
        .iter()
        .filter(|(name, entry)| looks_like_mmux(name, entry))
        .collect();
    matches.sort_by_key(|(name, _)| name.to_string());

    if matches.is_empty() {
        return MemoryReadiness::Missing { config: path };
    }
    if matches.len() > 1 {
        return MemoryReadiness::Ambiguous { config: path };
    }

    let (name, entry) = matches.remove(0);
    if entry.get("enabled").and_then(|value| value.as_bool()) == Some(false) {
        return MemoryReadiness::Disabled { config: path };
    }

    if let Err(reason) = validate_entry(entry) {
        return MemoryReadiness::Mismatched {
            config: path,
            reason,
        };
    }

    MemoryReadiness::Ready {
        server: name.clone(),
    }
}

fn looks_like_mmux(name: &str, entry: &serde_json::Value) -> bool {
    if name == "mmux" {
        return true;
    }
    entry
        .get("command")
        .and_then(|value| value.as_str())
        .and_then(|command| Path::new(command).file_name())
        .and_then(|name| name.to_str())
        == Some("mmux")
}

fn validate_entry(entry: &serde_json::Value) -> Result<(), String> {
    let Some(command) = entry.get("command").and_then(|value| value.as_str()) else {
        return Err("entry has no `command`; mmux is a stdio server".into());
    };
    if command.trim().is_empty() {
        return Err("entry has an empty `command`".into());
    }

    let args: Vec<&str> = entry
        .get("args")
        .and_then(|value| value.as_array())
        .map(|args| args.iter().filter_map(|arg| arg.as_str()).collect())
        .unwrap_or_default();
    if !args.iter().any(|arg| arg.trim() == "mcp") {
        return Err("`args` must contain `mcp`".into());
    }

    let exists = if Path::new(command).is_absolute() {
        Path::new(command).is_file()
    } else {
        which(command).is_some()
    };
    if !exists {
        return Err(format!(
            "`command` {command:?} does not resolve; a reinstall may have moved it"
        ));
    }

    match entry.get("exposure").and_then(|value| value.as_str()) {
        None => {
            return Err(
                "no `exposure`; pi defaults to `codemode`, so tools are not reachable \
                 from tool_search. Use \"deferred\" (or \"direct\" for both hot tools)"
                    .into(),
            );
        }
        Some("deferred") | Some("direct") => {}
        Some(other) => {
            return Err(format!(
                "`exposure` is {other:?}; use \"deferred\" so tools are discoverable"
            ));
        }
    }

    if let Some(tool_exposure) = entry.get("toolExposure")
        && !tool_exposure.is_object()
    {
        return Err("`toolExposure` must be an object".into());
    }

    Ok(())
}

/// Looks up an executable on `PATH`.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exposure: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "command": "mmux",
            "args": ["mcp"],
        });
        if let Some(exposure) = exposure {
            value["exposure"] = serde_json::Value::String(exposure.into());
        }
        value
    }

    #[test]
    fn validates_a_good_entry() {
        assert!(validate_entry(&entry(Some("deferred"))).is_ok());
        assert!(validate_entry(&entry(Some("direct"))).is_ok());
    }

    #[test]
    fn rejects_missing_exposure_and_bad_args() {
        assert!(
            validate_entry(&entry(None))
                .unwrap_err()
                .contains("codemode")
        );
        assert!(validate_entry(&entry(Some("hidden"))).is_err());
        let mut no_args = entry(Some("deferred"));
        no_args["args"] = serde_json::json!(["serve"]);
        assert!(validate_entry(&no_args).is_err());
    }

    #[test]
    fn detects_disabled_and_duplicate_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mcp.json");

        let config = MemoryConfig {
            pi_agent_dir: dir.path().to_path_buf(),
            ..MemoryConfig::default()
        };

        // No file at all -> Missing.
        assert!(matches!(check(&config), MemoryReadiness::Missing { .. }));

        fs::write(
            &path,
            serde_json::json!({
                "mcpServers": { "mmux": { "command": "mmux", "args": ["mcp"], "enabled": false } }
            })
            .to_string(),
        )
        .unwrap();
        assert!(matches!(check(&config), MemoryReadiness::Disabled { .. }));

        fs::write(
            &path,
            serde_json::json!({
                "mcpServers": {
                    "mmux": { "command": "mmux", "args": ["mcp"], "exposure": "deferred" },
                    "other": { "command": "/usr/local/bin/mmux", "args": ["mcp"], "exposure": "deferred" }
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(matches!(check(&config), MemoryReadiness::Ambiguous { .. }));

        fs::write(&path, "{ not json").unwrap();
        assert!(matches!(check(&config), MemoryReadiness::Invalid { .. }));
    }

    #[test]
    fn message_and_advice_are_present_for_failures() {
        let verdict = MemoryReadiness::Missing {
            config: PathBuf::from("/home/me/.pi/agent/mcp.json"),
        };
        assert!(verdict.message().unwrap().contains("not configured"));
        let advice = verdict.advice().unwrap();
        assert!(advice.contains("mcpServers"));
        assert!(advice.contains("pi mcp add mmux"));
        assert!(MemoryReadiness::NotApplicable.message().is_none());
    }

    #[test]
    fn unresolved_command_advice_mentions_path() {
        let verdict = MemoryReadiness::Mismatched {
            config: PathBuf::from("/home/me/.pi/agent/mcp.json"),
            reason: "`command` \"mmux\" does not resolve; a reinstall may have moved it".into(),
        };
        let advice = verdict.advice().unwrap();
        assert!(advice.contains("PATH"), "{advice}");
        assert!(advice.contains("mcpServers"));
    }

    #[test]
    fn entry_snippet_is_valid_json() {
        let wrapped = format!("{{{ENTRY_JSON}}}");
        let value: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(value["mmux"]["args"][0], "mcp");
    }
}
