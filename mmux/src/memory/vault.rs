//! The plaintext vault: workspace resolution, document files, and revisions.
//!
//! Layout (`<root>` is `$XDG_STATE_HOME/mmux/memory` by default):
//!
//! ```text
//! workspaces.json
//! workspaces/<basename>-<hash8>/
//!   workspace.json
//!   docs/<slug>.md
//!   _history/<slug>/<utc-nanos>.md
//! ```
//!
//! The registry is an optimization and a human index, not a source of truth:
//! deleting it loses only `last_seen` timestamps, and any workspace is
//! recreated the next time someone works in its directory.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::doc::{Doc, fnv64, is_valid_slug, slugify};
use crate::config::MemoryConfig;

const REGISTRY_FILE: &str = "workspaces.json";
const WORKSPACES_DIR: &str = "workspaces";
const WORKSPACE_FILE: &str = "workspace.json";
const DOCS_DIR: &str = "docs";
const HISTORY_DIR: &str = "_history";
const VERSION: u32 = 1;

/// An on-disk memory vault. Shared by the server, the CLI, and the MCP server.
#[derive(Debug, Clone)]
pub struct Memory {
    root: PathBuf,
    max_doc_bytes: usize,
    history_versions: usize,
}

/// One known working directory and its vault directory.
#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub key: String,
    pub path: PathBuf,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub key: String,
    pub path: String,
    pub created: String,
    pub last_seen: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Registry {
    version: u32,
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceFile {
    version: u32,
    path: String,
    created: String,
}

impl Memory {
    /// Builds the vault from configuration. `None` means memory is disabled or
    /// no state directory exists to put it in.
    pub fn from_config(config: &MemoryConfig, state_dir: Option<&Path>) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let root = match &config.dir {
            Some(dir) => expand(dir),
            None => state_dir?.join("memory"),
        };
        Some(Self {
            root,
            max_doc_bytes: config.max_doc_bytes,
            history_versions: config.history_versions,
        })
    }

    /// Explicit constructor for tests and for callers that already resolved a
    /// root.
    pub fn new(root: PathBuf) -> Self {
        let defaults = MemoryConfig::default();
        Self {
            root: expand(&root),
            max_doc_bytes: defaults.max_doc_bytes,
            history_versions: defaults.history_versions,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_doc_bytes(&self) -> usize {
        self.max_doc_bytes
    }

    /// Resolves the workspace for a working directory, creating its vault on
    /// first use. Matching is by longest known path prefix, so a document
    /// written from a subdirectory belongs to the project, not to the subdir.
    pub fn workspace_for(&self, cwd: &Path) -> Result<Workspace> {
        let canonical = canonical_or_absolute(cwd)?;
        let mut registry = self.load_registry()?;

        let exact = registry
            .workspaces
            .iter()
            .find(|record| Path::new(&record.path) == canonical)
            .map(|record| (record.key.clone(), record.path.clone()));
        if let Some((key, path)) = exact {
            return self.touch(registry, key, path);
        }

        let mut candidates: Vec<(usize, WorkspaceRecord)> = registry
            .workspaces
            .iter()
            .filter(|record| canonical.starts_with(Path::new(&record.path)))
            .map(|record| (record.path.len(), record.clone()))
            .collect();
        candidates.sort_by_key(|(length, _)| std::cmp::Reverse(*length));
        if let Some((_, record)) = candidates.into_iter().next() {
            return self.touch(registry, record.key, record.path);
        }

        let key = workspace_key(&canonical);
        let dir = self.workspace_dir(&key);
        create_private_dir(&dir)?;
        create_private_dir(&dir.join(DOCS_DIR))?;
        let now = super::doc::now();
        if !dir.join(WORKSPACE_FILE).exists() {
            let file = WorkspaceFile {
                version: VERSION,
                path: canonical.to_string_lossy().into_owned(),
                created: now.clone(),
            };
            atomic_write(
                &dir.join(WORKSPACE_FILE),
                serde_json::to_vec_pretty(&file)?.as_slice(),
            )?;
        }
        registry.workspaces.push(WorkspaceRecord {
            key: key.clone(),
            path: canonical.to_string_lossy().into_owned(),
            created: now.clone(),
            last_seen: now,
        });
        self.save_registry(&registry)?;
        Ok(Workspace {
            key,
            path: canonical,
            dir,
        })
    }

    fn touch(&self, mut registry: Registry, key: String, path: String) -> Result<Workspace> {
        let canonical = PathBuf::from(&path);
        let dir = self.workspace_dir(&key);
        create_private_dir(&dir)?;
        create_private_dir(&dir.join(DOCS_DIR))?;
        if let Some(record) = registry.workspaces.iter_mut().find(|r| r.key == key) {
            record.last_seen = super::doc::now();
        }
        // Best effort: a registry write failure must not block a read or a
        // write of the document itself.
        if let Err(err) = self.save_registry(&registry) {
            tracing::debug!(error = %err, "could not update workspace registry");
        }
        Ok(Workspace {
            key,
            path: canonical,
            dir,
        })
    }

    /// A workspace handle for reads: never creates a directory or rewrites the
    /// registry. An unknown directory still yields a stable key and path so a
    /// listing can answer "nothing here" without side effects.
    pub fn workspace_view(&self, cwd: &Path) -> Result<Workspace> {
        if let Some(workspace) = self.existing_workspace_for(cwd) {
            return Ok(workspace);
        }
        let canonical = canonical_or_absolute(cwd)?;
        let key = workspace_key(&canonical);
        Ok(Workspace {
            dir: self.workspace_dir(&key),
            key,
            path: canonical,
        })
    }

    /// Resolves a workspace without creating anything. Used by read-only views
    /// (for example the archive panel) that must not mutate the registry.
    pub fn existing_workspace_for(&self, cwd: &Path) -> Option<Workspace> {
        let canonical = canonical_or_absolute(cwd).ok()?;
        let registry = self.load_registry().ok()?;
        let mut candidates: Vec<&WorkspaceRecord> = registry
            .workspaces
            .iter()
            .filter(|record| canonical.starts_with(Path::new(&record.path)))
            .collect();
        candidates.sort_by_key(|record| std::cmp::Reverse(record.path.len()));
        let record = candidates.into_iter().next()?;
        let dir = self.workspace_dir(&record.key);
        dir.is_dir().then(|| Workspace {
            key: record.key.clone(),
            path: PathBuf::from(&record.path),
            dir,
        })
    }

    /// Workspaces that still have a vault directory, newest first.
    pub fn known_workspaces(&self) -> Result<Vec<Workspace>> {
        let mut registry = self.load_registry()?;
        registry
            .workspaces
            .sort_by(|left, right| right.last_seen.cmp(&left.last_seen));
        Ok(registry
            .workspaces
            .into_iter()
            .map(|record| Workspace {
                dir: self.workspace_dir(&record.key),
                key: record.key,
                path: PathBuf::from(record.path),
            })
            .filter(|workspace| workspace.dir.is_dir())
            .collect())
    }

    pub fn workspace_by_key(&self, key: &str) -> Result<Option<Workspace>> {
        let registry = self.load_registry()?;
        Ok(registry
            .workspaces
            .into_iter()
            .find(|record| record.key == key)
            .map(|record| Workspace {
                dir: self.workspace_dir(&record.key),
                key: record.key,
                path: PathBuf::from(record.path),
            }))
    }

    pub fn docs_dir(&self, workspace: &Workspace) -> PathBuf {
        workspace.dir.join(DOCS_DIR)
    }

    pub fn doc_path(&self, workspace: &Workspace, slug: &str) -> Result<PathBuf> {
        if !is_valid_slug(slug) {
            bail!("invalid document slug `{slug}`");
        }
        Ok(self.docs_dir(workspace).join(format!("{slug}.md")))
    }

    pub fn load(&self, workspace: &Workspace, slug: &str) -> Result<Doc> {
        let path = self.doc_path(workspace, slug)?;
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let doc = Doc::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        if doc.slug != slug {
            bail!("document slug does not match its file name");
        }
        Ok(doc)
    }

    /// Every readable document in a workspace. Unreadable files are logged and
    /// skipped: one damaged document must not hide the rest.
    pub fn list(&self, workspace: &Workspace) -> Result<Vec<Doc>> {
        let dir = self.docs_dir(workspace);
        let mut docs = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(docs),
            Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_valid_slug(slug) {
                tracing::debug!(path = %path.display(), "skipping non-slug document file");
                continue;
            }
            match self.load(workspace, slug) {
                Ok(doc) => docs.push(doc),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "skipping unreadable document");
                }
            }
        }
        docs.sort_by(|left, right| right.updated.cmp(&left.updated));
        Ok(docs)
    }

    /// Writes a document, keeping the previous version when history is enabled.
    pub fn store(&self, workspace: &Workspace, mut doc: Doc) -> Result<Doc> {
        let bytes = doc.render();
        if bytes.len() > self.max_doc_bytes {
            bail!(
                "document is {} bytes, above memory.max_doc_bytes ({})",
                bytes.len(),
                self.max_doc_bytes
            );
        }
        let path = self.doc_path(workspace, &doc.slug)?;
        if path.exists() {
            self.keep_history(workspace, &doc.slug, &path)?;
        }
        create_private_dir(&self.docs_dir(workspace))?;
        atomic_write(&path, bytes.as_bytes())?;
        // Report what is actually on disk, not what we intended to write.
        let text = fs::read_to_string(&path)?;
        doc.body = Doc::parse(&text)?.body;
        Ok(doc)
    }

    fn keep_history(&self, workspace: &Workspace, slug: &str, path: &Path) -> Result<()> {
        if self.history_versions == 0 {
            return Ok(());
        }
        let dir = workspace.dir.join(HISTORY_DIR).join(slug);
        create_private_dir(&dir)?;
        let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let target = dir.join(format!("{stamp}.md"));
        if let Err(err) = fs::copy(path, &target) {
            tracing::warn!(error = %err, "could not keep document history");
            return Ok(());
        }
        let mut versions: Vec<PathBuf> = match fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect(),
            Err(_) => return Ok(()),
        };
        versions.sort();
        let excess = versions.len().saturating_sub(self.history_versions);
        for stale in versions.into_iter().take(excess) {
            let _ = fs::remove_file(stale);
        }
        Ok(())
    }

    /// Revision timestamps, newest first.
    pub fn history(&self, workspace: &Workspace, slug: &str) -> Result<Vec<String>> {
        let dir = workspace.dir.join(HISTORY_DIR).join(slug);
        let mut stamps = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(stamps),
            Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                stamps.push(stem.to_owned());
            }
        }
        stamps.sort();
        stamps.reverse();
        Ok(stamps)
    }

    pub fn delete(&self, workspace: &Workspace, slug: &str) -> Result<()> {
        let path = self.doc_path(workspace, slug)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("no document named `{slug}`")
            }
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }

    pub fn doc_count(&self, workspace: &Workspace) -> usize {
        match fs::read_dir(self.docs_dir(workspace)) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
                .count(),
            Err(_) => 0,
        }
    }

    fn workspace_dir(&self, key: &str) -> PathBuf {
        self.root.join(WORKSPACES_DIR).join(key)
    }

    fn load_registry(&self) -> Result<Registry> {
        let path = self.root.join(REGISTRY_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Registry {
                version: VERSION,
                workspaces: Vec::new(),
            }),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save_registry(&self, registry: &Registry) -> Result<()> {
        create_private_dir(&self.root)?;
        let bytes = serde_json::to_vec_pretty(registry)?;
        atomic_write(&self.root.join(REGISTRY_FILE), &bytes)
    }
}

/// `basename-<8 hex of the canonical path>`, so two directories with the same
/// name never share a vault.
pub fn workspace_key(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let slug = slugify(name);
    let hash = fnv64(path.to_string_lossy().as_bytes());
    format!("{slug}-{:08x}", hash & 0xffff_ffff)
}

pub fn canonical_or_absolute(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(canonical) => Ok(canonical),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Ok(std::env::current_dir()?.join(path))
            }
        }
        Err(err) => Err(err).with_context(|| format!("resolving {}", path.display())),
    }
}

pub fn expand(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(shellexpand::tilde(&text).into_owned())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Write to a sibling temporary file, fsync, then rename. A crash leaves either
/// the old document or the new one, never a truncated file.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    create_private_dir(parent)?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{stamp}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mmux"),
        std::process::id()
    ));
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| {
        let _ = fs::remove_file(&temporary);
        format!("replacing {}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> (tempfile::TempDir, Memory) {
        let dir = tempfile::TempDir::new().unwrap();
        let memory = Memory::new(dir.path().join("memory"));
        (dir, memory)
    }

    #[test]
    fn workspace_keys_are_unique_per_path() {
        let one = workspace_key(Path::new("/home/me/src/api"));
        let two = workspace_key(Path::new("/home/me/work/api"));
        assert_eq!(one.rsplit('-').next().unwrap().len(), 8);
        assert_ne!(one, two);
        assert!(one.starts_with("api-"));
    }

    #[test]
    fn child_directories_resolve_to_the_parent_workspace() {
        let (tmp, memory) = memory();
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        let parent = memory.workspace_for(&project).unwrap();
        let child = memory.workspace_for(&project.join("src")).unwrap();
        assert_eq!(parent.key, child.key);
        assert_eq!(memory.known_workspaces().unwrap().len(), 1);
    }

    #[test]
    fn documents_round_trip_with_history() {
        let (tmp, memory) = memory();
        let workspace = memory.workspace_for(tmp.path()).unwrap();
        let mut doc = Doc {
            title: "A note".into(),
            slug: "a-note".into(),
            kind: super::super::doc::DocKind::Note,
            tags: Vec::new(),
            created: super::super::doc::now(),
            updated: super::super::doc::now(),
            author: "user".into(),
            source: None,
            paths: Vec::new(),
            supersedes: Vec::new(),
            body: "first".into(),
            extra: Default::default(),
        };
        memory.store(&workspace, doc.clone()).unwrap();
        doc.body = "second".into();
        memory.store(&workspace, doc.clone()).unwrap();

        assert_eq!(memory.load(&workspace, "a-note").unwrap().body, "second");
        assert_eq!(memory.list(&workspace).unwrap().len(), 1);
        assert_eq!(memory.history(&workspace, "a-note").unwrap().len(), 1);
        memory.delete(&workspace, "a-note").unwrap();
        assert!(memory.load(&workspace, "a-note").is_err());
        assert_eq!(memory.doc_count(&workspace), 0);
    }

    #[test]
    fn oversized_documents_are_rejected() {
        let (tmp, memory) = memory();
        let workspace = memory.workspace_for(tmp.path()).unwrap();
        let doc = Doc {
            title: "big".into(),
            slug: "big".into(),
            kind: super::super::doc::DocKind::Note,
            tags: Vec::new(),
            created: super::super::doc::now(),
            updated: super::super::doc::now(),
            author: "user".into(),
            source: None,
            paths: Vec::new(),
            supersedes: Vec::new(),
            body: "x".repeat(memory.max_doc_bytes() + 1),
            extra: Default::default(),
        };
        assert!(memory.store(&workspace, doc).is_err());
    }

    #[test]
    fn slug_paths_cannot_escape_the_docs_directory() {
        let (tmp, memory) = memory();
        let workspace = memory.workspace_for(tmp.path()).unwrap();
        assert!(memory.doc_path(&workspace, "../escape").is_err());
        assert!(memory.doc_path(&workspace, "a/b").is_err());
    }
}
