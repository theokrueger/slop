//! Project memory: a plaintext vault of meta-documents plus the MCP server that
//! exposes it to agents. There is no database and no derived index; the files
//! under [`Memory::root`] are the truth.

pub mod cli;
pub mod doc;
pub mod identity;
pub mod mcp;
pub mod ops;
pub mod readiness;
pub mod search;
pub mod vault;

pub use crate::config::MemoryConfig;
pub use doc::{Doc, DocKind, DocSource, DocSummary};
pub use identity::Origin;
pub use ops::{Author, Filters, Hit, NewDoc, Update};
pub use readiness::MemoryReadiness;
pub use vault::{Memory, Workspace};

/// Why a memory operation failed. The HTTP layer maps these onto status codes
/// and the MCP layer onto `isError` tool results.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("no document named `{0}`")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("project memory is disabled")]
    Disabled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

impl Memory {
    /// Loads a document, reporting a missing file as [`MemoryError::NotFound`]
    /// rather than an opaque I/O error.
    pub fn read(&self, workspace: &Workspace, slug: &str) -> Result<Doc> {
        let path = self
            .doc_path(workspace, slug)
            .map_err(|err| MemoryError::Invalid(err.to_string()))?;
        if !path.exists() {
            return Err(MemoryError::NotFound(slug.to_owned()));
        }
        self.load(workspace, slug).map_err(MemoryError::Other)
    }
}
