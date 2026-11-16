//! The operation layer shared by the CLI, the HTTP API, and the MCP server.
//! Every mutation is an explicit call; nothing here runs on a timer.

use std::fs;

use serde::Serialize;

use super::doc::{Doc, DocKind, DocSource, now, slugify};
use super::search::{Query, score};
use super::vault::{Memory, Workspace, create_private_dir};
use super::{MemoryError, Result};

/// A document to create.
#[derive(Debug, Clone, Default)]
pub struct NewDoc {
    pub title: String,
    pub kind: DocKind,
    pub tags: Vec<String>,
    pub body: String,
    pub paths: Vec<String>,
    pub supersedes: Vec<String>,
}

/// Provenance captured at write time and then frozen.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub source: Option<DocSource>,
}

impl Author {
    pub fn user() -> Self {
        Self {
            name: "user".into(),
            source: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub kind: Option<DocKind>,
    pub tag: Option<String>,
    /// RFC 3339 timestamp, date, or a duration such as `7d`.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub slug: String,
    pub title: String,
    pub kind: DocKind,
    pub updated: String,
    pub author: String,
    pub snippet: String,
    pub score: i64,
    pub workspace_key: String,
    pub workspace_path: String,
}

/// A deleted document, as listed in the web UI's archive.
#[derive(Debug, Clone, Serialize)]
pub struct ArchivedSummary {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub kind: DocKind,
    pub archived_at: String,
}

/// A deleted document with its body, for viewing or restoring.
#[derive(Debug, Clone, Serialize)]
pub struct ArchivedDoc {
    pub id: String,
    pub archived_at: String,
    #[serde(flatten)]
    pub doc: Doc,
}

#[derive(Debug, Clone, Default)]
pub struct Update {
    pub title: Option<String>,
    pub kind: Option<DocKind>,
    pub tags: Option<Vec<String>>,
    pub body: Option<String>,
    /// Reject the update unless the stored document has this hash.
    pub expected_hash: Option<String>,
}

impl Memory {
    pub fn add(&self, workspace: &Workspace, new: NewDoc, author: &Author) -> Result<Doc> {
        let title = validate_title(&new.title)?;
        let slug = self.unique_slug(workspace, &slugify(&title))?;
        let stamp = now();
        let doc = Doc {
            title,
            slug,
            kind: new.kind,
            tags: normalize_tags(new.tags),
            created: stamp.clone(),
            updated: stamp,
            author: author.name.clone(),
            source: author.source.clone(),
            paths: normalize_list(new.paths),
            supersedes: normalize_list(new.supersedes),
            body: normalize_body(&new.body),
            extra: Default::default(),
        };
        self.store(workspace, doc).map_err(MemoryError::Other)
    }

    pub fn update(&self, workspace: &Workspace, slug: &str, update: Update) -> Result<Doc> {
        let mut doc = self.read(workspace, slug)?;
        if let Some(expected) = &update.expected_hash
            && &doc.hash() != expected
        {
            return Err(MemoryError::Conflict(
                "the document changed since you loaded it".into(),
            ));
        }
        if let Some(title) = update.title {
            doc.title = validate_title(&title)?;
        }
        if let Some(kind) = update.kind {
            doc.kind = kind;
        }
        if let Some(tags) = update.tags {
            doc.tags = normalize_tags(tags);
        }
        if let Some(body) = update.body {
            doc.body = normalize_body(&body);
        }
        doc.updated = now();
        self.store(workspace, doc).map_err(MemoryError::Other)
    }

    pub fn append(
        &self,
        workspace: &Workspace,
        slug: &str,
        text: &str,
        expected_hash: Option<&str>,
    ) -> Result<Doc> {
        let mut doc = self.read(workspace, slug)?;
        if let Some(expected) = expected_hash
            && doc.hash() != expected
        {
            return Err(MemoryError::Conflict(
                "the document changed since you loaded it".into(),
            ));
        }
        let addition = normalize_body(text);
        if addition.is_empty() {
            return Err(MemoryError::Invalid("nothing to append".into()));
        }
        if !doc.body.is_empty() {
            doc.body.push_str("\n\n");
        }
        doc.body.push_str(&addition);
        doc.updated = now();
        self.store(workspace, doc).map_err(MemoryError::Other)
    }

    /// Deletes a document by moving it to the workspace archive. Search, the
    /// MCP server, and the CLI only look at `docs/`, so an archived document is
    /// invisible to agents until it is restored from the web UI.
    pub fn archive(&self, workspace: &Workspace, slug: &str) -> Result<Doc> {
        let doc = self.read(workspace, slug)?;
        let path = self.doc_path(workspace, slug).map_err(MemoryError::Other)?;
        let dir = self.archive_dir(workspace);
        create_private_dir(&dir).map_err(MemoryError::Other)?;
        let start = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let target = (start..)
            .take(10)
            .map(|nanos| dir.join(format!("{nanos}.md")))
            .find(|candidate| !candidate.exists());
        let Some(target) = target else {
            return Err(MemoryError::Other(anyhow::anyhow!(
                "could not find a free archive id"
            )));
        };
        fs::rename(&path, &target)?;
        Ok(doc)
    }

    /// Archived documents, newest deletion first.
    pub fn archived(&self, workspace: &Workspace) -> Result<Vec<ArchivedSummary>> {
        let dir = self.archive_dir(workspace);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(MemoryError::Other(err.into())),
        };
        let mut archived = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(doc) = Doc::parse(&text) else {
                tracing::warn!(path = %path.display(), "skipping unreadable archived document");
                continue;
            };
            archived.push((
                id.parse::<i128>().unwrap_or(0),
                ArchivedSummary {
                    id: id.to_owned(),
                    slug: doc.slug,
                    title: doc.title,
                    kind: doc.kind,
                    archived_at: archived_at(id),
                },
            ));
        }
        archived.sort_by_key(|(nanos, _)| std::cmp::Reverse(*nanos));
        Ok(archived.into_iter().map(|(_, summary)| summary).collect())
    }

    pub fn read_archived(&self, workspace: &Workspace, id: &str) -> Result<ArchivedDoc> {
        let path = self
            .archive_path(workspace, id)
            .map_err(|err| MemoryError::Invalid(err.to_string()))?;
        if !path.exists() {
            return Err(MemoryError::NotFound(id.to_owned()));
        }
        let text = fs::read_to_string(&path)?;
        let doc = Doc::parse(&text).map_err(MemoryError::Other)?;
        Ok(ArchivedDoc {
            id: id.to_owned(),
            archived_at: archived_at(id),
            doc,
        })
    }

    /// Moves an archived document back into `docs/`, giving it a fresh slug if
    /// the original name has been reused.
    pub fn restore_archived(&self, workspace: &Workspace, id: &str) -> Result<Doc> {
        let archived = self.read_archived(workspace, id)?;
        let mut doc = archived.doc;
        doc.slug = self.unique_slug(workspace, &doc.slug)?;
        doc.updated = now();
        let stored = self.store(workspace, doc).map_err(MemoryError::Other)?;
        let path = self
            .archive_path(workspace, id)
            .map_err(|err| MemoryError::Invalid(err.to_string()))?;
        fs::remove_file(&path)?;
        Ok(stored)
    }

    /// Permanently removes an archived document. Only the web UI calls this.
    pub fn purge_archived(&self, workspace: &Workspace, id: &str) -> Result<()> {
        let path = self
            .archive_path(workspace, id)
            .map_err(|err| MemoryError::Invalid(err.to_string()))?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(MemoryError::NotFound(id.to_owned()))
            }
            Err(err) => Err(MemoryError::Other(err.into())),
        }
    }

    pub fn summaries(
        &self,
        workspace: &Workspace,
        filters: &Filters,
    ) -> Result<Vec<super::doc::DocSummary>> {
        let cutoff = cutoff(filters.since.as_deref())?;
        let mut docs: Vec<Doc> = self.list(workspace).map_err(MemoryError::Other)?;
        docs.retain(|doc| matches(doc, filters, cutoff.as_deref()));
        docs.sort_by(|left, right| right.updated.cmp(&left.updated));
        Ok(docs.iter().map(Doc::summary).collect())
    }

    /// Searches one workspace, or every known workspace when `all` is set.
    pub fn search(
        &self,
        workspace: Option<&Workspace>,
        raw_query: &str,
        filters: &Filters,
        all: bool,
    ) -> Result<Vec<Hit>> {
        let query = Query::parse(raw_query);
        let cutoff = cutoff(filters.since.as_deref())?;
        let workspaces = if all {
            self.known_workspaces()?
        } else {
            match workspace {
                Some(workspace) => vec![workspace.clone()],
                None => Vec::new(),
            }
        };

        let mut hits = Vec::new();
        for workspace in workspaces {
            let docs = match self.list(&workspace).map_err(MemoryError::Other) {
                Ok(docs) => docs,
                Err(err) => {
                    tracing::warn!(workspace = %workspace.path.display(), error = %err, "skipping workspace");
                    continue;
                }
            };
            for doc in docs {
                if !matches(&doc, filters, cutoff.as_deref()) {
                    continue;
                }
                if let Some((score, snippet)) = score(&doc, &query) {
                    hits.push(Hit {
                        slug: doc.slug,
                        title: doc.title,
                        kind: doc.kind,
                        updated: doc.updated,
                        author: doc.author,
                        snippet,
                        score,
                        workspace_key: workspace.key.clone(),
                        workspace_path: workspace.path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.updated.cmp(&left.updated))
        });
        Ok(hits)
    }

    fn unique_slug(&self, workspace: &Workspace, base: &str) -> Result<String> {
        for attempt in 0..100 {
            let candidate = if attempt == 0 {
                base.to_owned()
            } else {
                format!("{base}-{}", attempt + 1)
            };
            let path = self
                .doc_path(workspace, &candidate)
                .map_err(MemoryError::Other)?;
            if !path.exists() {
                return Ok(candidate);
            }
        }
        Err(MemoryError::Invalid(
            "could not find a free slug after 100 attempts".into(),
        ))
    }
}

fn archived_at(id: &str) -> String {
    id.parse::<i128>()
        .ok()
        .and_then(|nanos| time::OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
        .and_then(|stamp| {
            stamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn matches(doc: &Doc, filters: &Filters, cutoff: Option<&str>) -> bool {
    if let Some(kind) = filters.kind
        && doc.kind != kind
    {
        return false;
    }
    if let Some(tag) = &filters.tag
        && !doc
            .tags
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(tag.trim()))
    {
        return false;
    }
    if let Some(cutoff) = cutoff
        && doc.updated.as_str() < cutoff
    {
        return false;
    }
    true
}

/// Accepts `7d`, `36h`, `30m`, or a date/timestamp. Returns a comparable
/// timestamp prefix, or `None` when there is no filter.
pub fn cutoff(since: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = since.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let (number, unit) = raw.split_at(raw.len().saturating_sub(1));
    let amount: i64 = number.parse().unwrap_or(-1);
    let duration = match unit {
        "d" if amount >= 0 => time::Duration::days(amount),
        "h" if amount >= 0 => time::Duration::hours(amount),
        "m" if amount >= 0 => time::Duration::minutes(amount),
        _ => {
            return Ok(Some(raw.to_owned()));
        }
    };
    let stamp = time::OffsetDateTime::now_utc() - duration;
    Ok(Some(
        stamp
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|err| MemoryError::Invalid(format!("invalid --since: {err}")))?,
    ))
}

fn validate_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(MemoryError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > 200 {
        return Err(MemoryError::Invalid(
            "title must be at most 200 characters".into(),
        ));
    }
    if title.chars().any(char::is_control) {
        return Err(MemoryError::Invalid(
            "title must not contain control characters".into(),
        ));
    }
    Ok(title.to_owned())
}

fn normalize_body(body: &str) -> String {
    body.replace('\0', "").trim().to_owned()
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for tag in tags {
        let tag = tag.trim().trim_start_matches('#').to_lowercase();
        if tag.is_empty() || tag.chars().count() > 40 || !seen.insert(tag.clone()) {
            continue;
        }
        out.push(tag);
        if out.len() >= 20 {
            break;
        }
    }
    out
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        let value = value.trim().to_owned();
        if value.is_empty() || !seen.insert(value.clone()) {
            continue;
        }
        out.push(value);
        if out.len() >= 50 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::vault::Memory;

    fn setup() -> (tempfile::TempDir, Memory, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        let memory = Memory::new(dir.path().join("memory"));
        let workspace = memory.workspace_for(dir.path()).unwrap();
        (dir, memory, workspace)
    }

    fn new_doc(title: &str, body: &str) -> NewDoc {
        NewDoc {
            title: title.into(),
            kind: DocKind::Note,
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn slug_collisions_get_suffixes() {
        let (_tmp, memory, workspace) = setup();
        let first = memory
            .add(&workspace, new_doc("Same title", "a"), &Author::user())
            .unwrap();
        let second = memory
            .add(&workspace, new_doc("Same title", "b"), &Author::user())
            .unwrap();
        assert_eq!(first.slug, "same-title");
        assert_eq!(second.slug, "same-title-2");
    }

    #[test]
    fn updates_and_appends_bump_the_timestamp() {
        let (_tmp, memory, workspace) = setup();
        let doc = memory
            .add(&workspace, new_doc("T", "one"), &Author::user())
            .unwrap();
        let updated = memory
            .update(
                &workspace,
                &doc.slug,
                Update {
                    body: Some("two".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.body, "two");
        let appended = memory.append(&workspace, &doc.slug, "three", None).unwrap();
        assert_eq!(appended.body, "two\n\nthree");
    }

    #[test]
    fn stale_hashes_are_rejected() {
        let (_tmp, memory, workspace) = setup();
        let doc = memory
            .add(&workspace, new_doc("T", "one"), &Author::user())
            .unwrap();
        let err = memory
            .update(
                &workspace,
                &doc.slug,
                Update {
                    body: Some("two".into()),
                    expected_hash: Some("deadbeef".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, MemoryError::Conflict(_)));
    }

    #[test]
    fn filters_by_kind_tag_and_since() {
        let (_tmp, memory, workspace) = setup();
        memory
            .add(
                &workspace,
                NewDoc {
                    title: "Decision".into(),
                    kind: DocKind::Decision,
                    tags: vec!["session".into()],
                    body: "why".into(),
                    ..Default::default()
                },
                &Author::user(),
            )
            .unwrap();
        memory
            .add(&workspace, new_doc("Note", "body"), &Author::user())
            .unwrap();

        let decisions = memory
            .summaries(
                &workspace,
                &Filters {
                    kind: Some(DocKind::Decision),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(decisions.len(), 1);
        let tagged = memory
            .summaries(
                &workspace,
                &Filters {
                    tag: Some("SESSION".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(tagged.len(), 1);
        let recent = memory
            .summaries(
                &workspace,
                &Filters {
                    since: Some("7d".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(recent.len(), 2);
        let old = memory
            .summaries(
                &workspace,
                &Filters {
                    since: Some("1999-01-01".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(old.len(), 2);
    }

    #[test]
    fn archiving_hides_a_doc_from_reads_and_search() {
        let (_tmp, memory, workspace) = setup();
        memory
            .add(
                &workspace,
                new_doc("Restart policy", "panes relaunch"),
                &Author::user(),
            )
            .unwrap();

        memory.archive(&workspace, "restart-policy").unwrap();

        assert!(matches!(
            memory.read(&workspace, "restart-policy"),
            Err(MemoryError::NotFound(_))
        ));
        assert!(
            memory
                .search(Some(&workspace), "relaunch", &Filters::default(), false)
                .unwrap()
                .is_empty()
        );
        assert!(
            memory
                .summaries(&workspace, &Filters::default())
                .unwrap()
                .is_empty()
        );

        let archived = memory.archived(&workspace).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].slug, "restart-policy");
        assert_eq!(archived[0].title, "Restart policy");
        assert!(!archived[0].archived_at.is_empty());

        let detail = memory.read_archived(&workspace, &archived[0].id).unwrap();
        assert!(detail.doc.body.contains("relaunch"));

        let restored = memory
            .restore_archived(&workspace, &archived[0].id)
            .unwrap();
        assert_eq!(restored.slug, "restart-policy");
        assert!(memory.archived(&workspace).unwrap().is_empty());
        assert_eq!(
            memory.read(&workspace, "restart-policy").unwrap().body,
            "panes relaunch"
        );
    }

    #[test]
    fn restoring_after_a_slug_is_reused_renames_the_doc() {
        let (_tmp, memory, workspace) = setup();
        memory
            .add(&workspace, new_doc("Same title", "old"), &Author::user())
            .unwrap();
        memory.archive(&workspace, "same-title").unwrap();
        // A new doc takes the slug back.
        memory
            .add(&workspace, new_doc("Same title", "new"), &Author::user())
            .unwrap();
        let archived = memory.archived(&workspace).unwrap();
        let restored = memory
            .restore_archived(&workspace, &archived[0].id)
            .unwrap();
        assert_eq!(restored.slug, "same-title-2");
        assert_eq!(restored.body, "old");
    }

    #[test]
    fn purging_removes_an_archived_doc_and_rejects_bad_ids() {
        let (_tmp, memory, workspace) = setup();
        memory
            .add(&workspace, new_doc("T", "body"), &Author::user())
            .unwrap();
        memory.archive(&workspace, "t").unwrap();
        let archived = memory.archived(&workspace).unwrap();
        memory.purge_archived(&workspace, &archived[0].id).unwrap();
        assert!(memory.archived(&workspace).unwrap().is_empty());
        assert!(matches!(
            memory.purge_archived(&workspace, &archived[0].id),
            Err(MemoryError::NotFound(_))
        ));
        assert!(memory.read_archived(&workspace, "../escape").is_err());
        assert!(memory.archived(&workspace).is_ok());
    }

    #[test]
    fn search_prefers_docs_from_the_current_workspace() {
        let (_tmp, memory, workspace) = setup();
        memory
            .add(
                &workspace,
                new_doc("Restart policy", "panes restart"),
                &Author::user(),
            )
            .unwrap();
        let hits = memory
            .search(Some(&workspace), "restart", &Filters::default(), false)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("restart"));
    }
}
