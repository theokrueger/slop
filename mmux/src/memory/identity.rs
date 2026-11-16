//! Inferring document provenance. `author` is recorded, never trusted for
//! authorization.

use super::doc::DocSource;
use super::ops::Author;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The browser UI acts as the signed-in user.
    Ui,
    /// `mmux mem` in a shell.
    Cli,
    /// A tool call arriving through the MCP server.
    Mcp,
}

pub fn author(origin: Origin) -> Author {
    match origin {
        Origin::Ui => Author::user(),
        Origin::Cli | Origin::Mcp => from_environment(),
    }
}

fn from_environment() -> Author {
    if let Some(agent) = env_value("MMUX_AGENT") {
        let source = DocSource {
            session: env_value("MMUX_SESSION_ID"),
            pane: env_value("MMUX_PANE_SLOT"),
        };
        return Author {
            name: format!("agent:{agent}"),
            source: (source.session.is_some() || source.pane.is_some()).then_some(source),
        };
    }

    // pi exports this into its own process environment, so MCP children
    // inherit it even though `PI_SESSION_ID` is deliberately absent.
    if env_value("PI_CODING_AGENT").as_deref() == Some("true") {
        return Author {
            name: "agent:pi".into(),
            source: None,
        };
    }

    Author {
        name: format!("cli:{}", username()),
        source: None,
    }
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn username() -> String {
    for key in ["USER", "LOGNAME", "USERNAME"] {
        if let Some(value) = env_value(key) {
            return value;
        }
    }
    "unknown".to_owned()
}
