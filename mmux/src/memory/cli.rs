//! `mmux mem …` and `mmux mcp` — the human and agent entry points.
//!
//! Both load the same `mmux.toml` as the server. Settings never travel through
//! the environment; the only environment read here is identity (`MMUX_*`,
//! `PI_CODING_AGENT`) for document provenance.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::config::Config;
use crate::memory::{
    DocKind, Filters, Memory, MemoryReadiness, NewDoc, Origin, Update, identity, mcp, readiness,
};
use crate::persistence;

#[derive(Debug, Args)]
pub struct MemArgs {
    /// Workspace to use; defaults to the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    #[command(subcommand)]
    pub action: MemAction,
}

#[derive(Debug, Subcommand)]
pub enum MemAction {
    /// Save a new document.
    Add(AddArgs),
    /// Replace a document body (and optionally its title, kind, or tags).
    Edit(EditArgs),
    /// Append a section to an existing document.
    Append(AppendArgs),
    /// Delete a document permanently.
    Rm {
        slug: String,
        /// Required: deletion is never implicit.
        #[arg(long)]
        yes: bool,
    },
    /// List documents in the workspace.
    List(ListArgs),
    /// Print a document.
    Show {
        slug: String,
        #[arg(long)]
        json: bool,
        /// Print the file exactly as stored.
        #[arg(long)]
        raw: bool,
    },
    /// Search documents; docs from every workspace with --all-workspaces.
    Search(SearchArgs),
    /// List saved revisions of a document.
    History {
        slug: String,
        #[arg(long)]
        json: bool,
    },
    /// Report whether pi can see the memory MCP server.
    Status {
        #[arg(long)]
        json: bool,
        /// Re-run the probe (it is read-only).
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Debug, Args)]
pub struct AddArgs {
    #[arg(long)]
    pub title: String,
    #[arg(long, value_parser = parse_kind, default_value = "note")]
    pub kind: DocKind,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    #[arg(long = "path")]
    pub paths: Vec<String>,
    #[arg(long = "supersedes")]
    pub supersedes: Vec<String>,
    /// Read the body from this file; `-` or omission reads stdin.
    #[arg(long, value_name = "FILE")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    pub slug: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long, value_parser = parse_kind)]
    pub kind: Option<DocKind>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Read the body from this file; `-` or omission reads stdin.
    #[arg(long, value_name = "FILE")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct AppendArgs {
    pub slug: String,
    #[arg(long, value_name = "FILE")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long, value_parser = parse_kind)]
    pub kind: Option<DocKind>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Terms to AND; quote a phrase; prefix a term with - to exclude.
    #[arg(trailing_var_arg = true)]
    pub query: Vec<String>,
    #[arg(long, value_parser = parse_kind)]
    pub kind: Option<DocKind>,
    #[arg(long)]
    pub tag: Option<String>,
    /// Duration (7d, 12h) or a date (2026-01-01).
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub all_workspaces: bool,
    #[arg(long)]
    pub json: bool,
}

pub fn run(config: &Config, args: MemArgs) -> Result<()> {
    let memory = memory(config)?;
    let cwd = match args.workspace {
        Some(path) => path,
        None => std::env::current_dir().context("could not determine the current directory")?,
    };

    // Status is about the pi integration, not a workspace, and must never
    // create one as a side effect.
    match args.action {
        MemAction::Status { json, .. } => {
            let verdict = readiness::check(&config.memory);
            print_status(&verdict, json)
        }
        action => run_action(&memory, &cwd, action),
    }
}

pub fn run_mcp(config: &Config) -> Result<()> {
    let memory = memory(config)?;
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    mcp::Server::new(memory, config.memory.agent_writes, cwd).serve()
}

fn memory(config: &Config) -> Result<Memory> {
    let state_dir = persistence::default_state_dir();
    Memory::from_config(&config.memory, state_dir.as_deref())
        .context("project memory is disabled (set [memory] enabled = true)")
}

fn run_action(memory: &Memory, cwd: &Path, action: MemAction) -> Result<()> {
    let author = identity::author(Origin::Cli);
    match action {
        MemAction::Add(args) => {
            let workspace = memory.workspace_for(cwd)?;
            let body = read_body(args.file.as_deref())?;
            let doc = memory.add(
                &workspace,
                NewDoc {
                    title: args.title,
                    kind: args.kind,
                    tags: args.tags,
                    body,
                    paths: args.paths,
                    supersedes: args.supersedes,
                },
                &author,
            )?;
            println!("{}", doc.slug);
        }
        MemAction::Edit(args) => {
            let workspace = memory.workspace_for(cwd)?;
            let body = Some(read_body(args.file.as_deref())?);
            let doc = memory.update(
                &workspace,
                &args.slug,
                Update {
                    title: args.title,
                    kind: args.kind,
                    tags: (!args.tags.is_empty()).then_some(args.tags),
                    body,
                    expected_hash: None,
                },
            )?;
            println!("{}", doc.slug);
        }
        MemAction::Append(args) => {
            let workspace = memory.workspace_for(cwd)?;
            let body = read_body(args.file.as_deref())?;
            let doc = memory.append(&workspace, &args.slug, &body, None)?;
            println!("{}", doc.slug);
        }
        MemAction::Rm { slug, yes } => {
            if !yes {
                bail!("refusing to delete `{slug}` without --yes");
            }
            let workspace = memory.workspace_for(cwd)?;
            memory.remove(&workspace, &slug)?;
            println!("deleted {slug}");
        }
        MemAction::List(args) => {
            let summaries = match memory.existing_workspace_for(cwd) {
                Some(workspace) => memory.summaries(
                    &workspace,
                    &Filters {
                        kind: args.kind,
                        tag: args.tag,
                        since: None,
                    },
                )?,
                None => Vec::new(),
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&summaries)?);
            } else if summaries.is_empty() {
                println!("no documents in {}", cwd.display());
            } else {
                println!("{:<28} {:<10} {:<20} TITLE", "SLUG", "KIND", "UPDATED");
                for doc in summaries {
                    println!(
                        "{:<28} {:<10} {:<20} {}",
                        doc.slug, doc.kind, doc.updated, doc.title
                    );
                }
            }
        }
        MemAction::Show { slug, json, raw } => {
            let workspace = memory
                .existing_workspace_for(cwd)
                .with_context(|| format!("no memory documents for {}", cwd.display()))?;
            if json {
                let doc = memory.read(&workspace, &slug)?;
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else if raw {
                let doc = memory.read(&workspace, &slug)?;
                print!("{}", doc.render());
            } else {
                let doc = memory.read(&workspace, &slug)?;
                println!("# {} [{}] ({})", doc.title, doc.kind, doc.slug);
                println!(
                    "updated {} · author {} · {} tags",
                    doc.updated,
                    doc.author,
                    doc.tags.len()
                );
                if !doc.tags.is_empty() {
                    println!("tags: {}", doc.tags.join(", "));
                }
                println!();
                println!("{}", doc.body);
            }
        }
        MemAction::Search(args) => {
            let query = args.query.join(" ");
            let filters = Filters {
                kind: args.kind,
                tag: args.tag,
                since: args.since,
            };
            let workspace = memory.existing_workspace_for(cwd);
            let hits = memory.search(workspace.as_ref(), &query, &filters, args.all_workspaces)?;
            let hits: Vec<_> = hits.into_iter().take(args.limit.max(1)).collect();
            if args.json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                let label = workspace
                    .map(|workspace| workspace.path)
                    .unwrap_or_else(|| cwd.to_path_buf());
                println!("no matches in {}", label.display());
            } else {
                for hit in hits {
                    println!(
                        "{} [{}] {} ({})",
                        hit.title, hit.kind, hit.slug, hit.updated
                    );
                    println!("  {}", hit.snippet);
                    if args.all_workspaces {
                        println!("  {}", hit.workspace_path);
                    }
                }
            }
        }
        MemAction::History { slug, json } => {
            let stamps = match memory.existing_workspace_for(cwd) {
                Some(workspace) => memory.history(&workspace, &slug)?,
                None => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&stamps)?);
            } else if stamps.is_empty() {
                println!("no revisions for {slug}");
            } else {
                for stamp in stamps {
                    println!("{stamp}");
                }
            }
        }
        // Handled in `run`, before a workspace is resolved.
        MemAction::Status { .. } => {}
    }
    Ok(())
}

fn print_status(readiness: &MemoryReadiness, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "state": readiness.state_name(),
                "message": readiness.message(),
                "advice": readiness.advice(),
            }))?
        );
        return Ok(());
    }
    match readiness {
        MemoryReadiness::Ready { server } => {
            println!("memory MCP server ready (pi entry `{server}`)")
        }
        MemoryReadiness::NotApplicable => {
            println!("not applicable: pi is not installed or memory is disabled")
        }
        _ => {
            if let Some(message) = readiness.message() {
                println!("{message}");
            }
            if let Some(advice) = readiness.advice() {
                println!("\n{advice}");
            }
        }
    }
    Ok(())
}

fn read_body(file: Option<&Path>) -> Result<String> {
    match file {
        Some(path) if path != Path::new("-") => {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
        }
        _ => {
            let mut body = String::new();
            std::io::stdin()
                .read_to_string(&mut body)
                .context("reading the body from stdin")?;
            Ok(body)
        }
    }
}

fn parse_kind(value: &str) -> std::result::Result<DocKind, String> {
    DocKind::parse(value).map_err(|err| err.to_string())
}
