//! Memory documents: TOML frontmatter plus a markdown body.
//!
//! The file is the source of truth. Nothing here derives state that cannot be
//! rebuilt by re-reading the vault, and unknown frontmatter keys survive a
//! round-trip so a UI save never damages a hand-edited document.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const FENCE: &str = "+++";

/// Current UTC time as RFC 3339. Document timestamps are strings on purpose:
/// the file stays greppable and readable by any tool.
pub fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
const FENCE_BYTES: usize = 3;

/// What kind of meta-document this is. Kinds are a fixed allowlist so the UI
/// and search can group without maintaining a taxonomy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    #[default]
    Note,
    Decision,
    Plan,
    Runbook,
    Retro,
    Reference,
}

impl DocKind {
    pub const ALL: [DocKind; 6] = [
        DocKind::Note,
        DocKind::Decision,
        DocKind::Plan,
        DocKind::Runbook,
        DocKind::Retro,
        DocKind::Reference,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DocKind::Note => "note",
            DocKind::Decision => "decision",
            DocKind::Plan => "plan",
            DocKind::Runbook => "runbook",
            DocKind::Retro => "retro",
            DocKind::Reference => "reference",
        }
    }

    /// Small ranking bonus for kinds that answer "why" and "what next".
    pub const fn weight(self) -> i64 {
        match self {
            DocKind::Decision => 3,
            DocKind::Plan => 2,
            DocKind::Runbook => 2,
            DocKind::Retro => 1,
            DocKind::Reference => 1,
            DocKind::Note => 0,
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "note" => Ok(DocKind::Note),
            "decision" => Ok(DocKind::Decision),
            "plan" => Ok(DocKind::Plan),
            "runbook" => Ok(DocKind::Runbook),
            "retro" => Ok(DocKind::Retro),
            "reference" => Ok(DocKind::Reference),
            other => bail!(
                "unknown kind `{other}` (expected one of: {})",
                DocKind::ALL
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl fmt::Display for DocKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a document came from. Provenance only; never used for authorization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Doc {
    pub title: String,
    pub slug: String,
    pub kind: DocKind,
    pub tags: Vec<String>,
    pub created: String,
    pub updated: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DocSource>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    pub body: String,
    /// Frontmatter keys this version does not know about.
    #[serde(skip)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// The list view: everything except the body, so a list stays small.
#[derive(Debug, Clone, Serialize)]
pub struct DocSummary {
    pub slug: String,
    pub title: String,
    pub kind: DocKind,
    pub tags: Vec<String>,
    pub created: String,
    pub updated: String,
    pub author: String,
}

impl Doc {
    pub fn summary(&self) -> DocSummary {
        DocSummary {
            slug: self.slug.clone(),
            title: self.title.clone(),
            kind: self.kind,
            tags: self.tags.clone(),
            created: self.created.clone(),
            updated: self.updated.clone(),
            author: self.author.clone(),
        }
    }

    /// Parses a document. The first non-empty line must be `+++`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let start = text
            .find(FENCE)
            .filter(|&index| text[..index].trim().is_empty())
            .context("missing `+++` frontmatter")?;
        let after_open = start + FENCE_BYTES;
        let rest = &text[after_open..];
        let end = rest.find(FENCE).context("unterminated `+++` frontmatter")?;
        let frontmatter = &rest[..end];
        // `render` trims the body and adds one trailing newline; parse applies
        // the same normalization so a document survives a round-trip byte for
        // byte and `store` can report what is actually on disk.
        let body = rest[end + FENCE_BYTES..]
            .trim_start_matches(['\r', '\n'])
            .trim_end()
            .to_owned();

        let mut table: toml::Table =
            toml::from_str(frontmatter).context("invalid TOML frontmatter")?;

        let title = take_string(&mut table, "title")?;
        let slug = take_string(&mut table, "slug")?;
        let kind = DocKind::parse(&take_string(&mut table, "kind")?)?;
        let tags = take_strings(&mut table, "tags")?;
        let created = take_string_or(&mut table, "created", "");
        let updated = take_string_or(&mut table, "updated", "");
        let author = take_string_or(&mut table, "author", "unknown");
        let source = take_source(&mut table)?;
        let paths = take_strings(&mut table, "paths")?;
        let supersedes = take_strings(&mut table, "supersedes")?;

        if !is_valid_slug(&slug) {
            bail!("invalid slug `{slug}` in frontmatter");
        }

        Ok(Self {
            title,
            slug,
            kind,
            tags,
            created,
            updated,
            author,
            source,
            paths,
            supersedes,
            body,
            extra: table.into_iter().collect(),
        })
    }

    /// Renders the canonical on-disk form. Always ends with a newline and has
    /// exactly one blank line between frontmatter and body.
    pub fn render(&self) -> String {
        let mut table = toml::Table::new();
        table.insert("title".into(), toml::Value::String(self.title.clone()));
        table.insert("slug".into(), toml::Value::String(self.slug.clone()));
        table.insert(
            "kind".into(),
            toml::Value::String(self.kind.as_str().to_owned()),
        );
        table.insert(
            "tags".into(),
            toml::Value::Array(
                self.tags
                    .iter()
                    .map(|tag| toml::Value::String(tag.clone()))
                    .collect(),
            ),
        );
        table.insert("created".into(), toml::Value::String(self.created.clone()));
        table.insert("updated".into(), toml::Value::String(self.updated.clone()));
        table.insert("author".into(), toml::Value::String(self.author.clone()));
        if let Some(source) = &self.source {
            let mut entry = toml::Table::new();
            if let Some(session) = &source.session {
                entry.insert("session".into(), toml::Value::String(session.clone()));
            }
            if let Some(pane) = &source.pane {
                entry.insert("pane".into(), toml::Value::String(pane.clone()));
            }
            if !entry.is_empty() {
                table.insert("source".into(), toml::Value::Table(entry));
            }
        }
        if !self.paths.is_empty() {
            table.insert(
                "paths".into(),
                toml::Value::Array(
                    self.paths
                        .iter()
                        .map(|path| toml::Value::String(path.clone()))
                        .collect(),
                ),
            );
        }
        if !self.supersedes.is_empty() {
            table.insert(
                "supersedes".into(),
                toml::Value::Array(
                    self.supersedes
                        .iter()
                        .map(|value| toml::Value::String(value.clone()))
                        .collect(),
                ),
            );
        }
        // Unknown keys keep their position after the known ones; known keys
        // always win if a hand edit duplicated one.
        for (key, value) in &self.extra {
            table.entry(key.clone()).or_insert_with(|| value.clone());
        }

        let frontmatter = toml::to_string_pretty(&table).unwrap_or_default();
        let body = self.body.trim_end();
        format!("{}\n{frontmatter}{}\n\n{body}\n", FENCE, FENCE)
    }

    /// Content hash used for optimistic concurrency (`If-Match`).
    pub fn hash(&self) -> String {
        content_hash(self.render().as_bytes())
    }
}

fn take_string(table: &mut toml::Table, key: &str) -> Result<String> {
    match table.remove(key) {
        Some(toml::Value::String(value)) => Ok(value),
        Some(_) => bail!("frontmatter `{key}` must be a string"),
        None => bail!("frontmatter is missing `{key}`"),
    }
}

fn take_string_or(table: &mut toml::Table, key: &str, fallback: &str) -> String {
    match table.remove(key) {
        Some(toml::Value::String(value)) => value,
        _ => fallback.to_owned(),
    }
}

fn take_strings(table: &mut toml::Table, key: &str) -> Result<Vec<String>> {
    match table.remove(key) {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                toml::Value::String(value) => Ok(value),
                _ => bail!("frontmatter `{key}` must contain strings"),
            })
            .collect(),
        Some(_) => bail!("frontmatter `{key}` must be an array"),
    }
}

fn take_source(table: &mut toml::Table) -> Result<Option<DocSource>> {
    match table.remove("source") {
        None => Ok(None),
        Some(toml::Value::Table(mut entry)) => Ok(Some(DocSource {
            session: entry
                .remove("session")
                .and_then(|value| value.as_str().map(str::to_owned)),
            pane: entry
                .remove("pane")
                .and_then(|value| value.as_str().map(str::to_owned)),
        })),
        Some(_) => bail!("frontmatter `source` must be a table"),
    }
}

/// Slugs address files, so the alphabet is deliberately tiny.
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 80
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
}

/// Deterministic, human-readable slug. Non-ASCII characters are dropped, so a
/// title in another script still produces a stable ASCII file name.
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in title.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = !out.is_empty();
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        "doc".to_owned()
    } else {
        out
    }
}

/// FNV-1a. Used for short, stable identifiers and conflict detection; never
/// for anything that needs cryptographic strength.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Doc {
        Doc {
            title: "Why panes restart".into(),
            slug: "why-panes-restart".into(),
            kind: DocKind::Decision,
            tags: vec!["session".into(), "restart".into()],
            created: "2026-11-16T00:00:00Z".into(),
            updated: "2026-11-16T00:00:00Z".into(),
            author: "agent:pi".into(),
            source: Some(DocSource {
                session: Some("01a1e1ab".into()),
                pane: Some("bottom-left".into()),
            }),
            paths: vec!["src/session/pane.rs".into()],
            supersedes: Vec::new(),
            body: "Runners are relaunched because a PTY cannot be serialized.".into(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn round_trips_through_the_file_format() {
        let doc = sample();
        let parsed = Doc::parse(&doc.render()).unwrap();
        assert_eq!(parsed, doc);
    }

    #[test]
    fn keeps_unknown_frontmatter_keys() {
        let text = "+++\ntitle = \"t\"\nslug = \"t\"\nkind = \"note\"\nfuture = 42\n+++\n\nbody\n";
        let doc = Doc::parse(text).unwrap();
        assert_eq!(doc.extra.get("future"), Some(&toml::Value::Integer(42)));
        let rendered = doc.render();
        assert!(rendered.contains("future = 42"), "{rendered}");
        assert_eq!(Doc::parse(&rendered).unwrap().extra, doc.extra);
    }

    #[test]
    fn rejects_bad_slugs_and_missing_keys() {
        let bad = "+++\ntitle = \"t\"\nslug = \"../etc/passwd\"\nkind = \"note\"\n+++\n";
        assert!(Doc::parse(bad).is_err());
        let missing = "+++\nslug = \"t\"\nkind = \"note\"\n+++\n";
        assert!(Doc::parse(missing).is_err());
        let bad_kind = "+++\ntitle = \"t\"\nslug = \"t\"\nkind = \"poem\"\n+++\n";
        assert!(Doc::parse(bad_kind).is_err());
    }

    #[test]
    fn slugs_are_stable_and_safe() {
        assert_eq!(slugify("Why panes restart?"), "why-panes-restart");
        assert_eq!(slugify("  ---  "), "doc");
        assert_eq!(slugify("Über alles"), "ber-alles");
        assert!(is_valid_slug(&slugify("a/b")));
    }

    #[test]
    fn hash_changes_with_content() {
        let doc = sample();
        let first = doc.hash();
        let mut other = doc.clone();
        other.body.push_str("More.\n");
        assert_ne!(first, other.hash());
        assert_eq!(first, sample().hash());
    }
}
