//! A small stdio MCP server: newline-delimited JSON-RPC 2.0, tools only.
//!
//! stdout carries protocol messages and nothing else; all logging goes to
//! stderr through `tracing`. The server is stateless apart from the working
//! directory it was started in, which decides the workspace.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use super::doc::{DocKind, is_valid_slug};
use super::{Memory, MemoryError, Workspace};
use crate::config::AgentWrites;

/// Versions pi accepts; echoing one of them keeps the handshake valid.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LIMIT: usize = 50;
const MAX_SEARCH_TEXT: usize = 20_000;

pub struct Server {
    memory: Memory,
    agent_writes: AgentWrites,
    cwd: PathBuf,
}

impl Server {
    pub fn new(memory: Memory, agent_writes: AgentWrites, cwd: PathBuf) -> Self {
        Self {
            memory,
            agent_writes,
            cwd,
        }
    }

    /// Runs until stdin closes. Errors on stdin are fatal; a malformed message
    /// only produces a JSON-RPC error.
    pub fn serve(self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        tracing::info!(cwd = %self.cwd.display(), "memory MCP server ready");
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Value>(&line) {
                Ok(request) => self.handle(&request),
                Err(err) => Some(error_response(
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                )),
            };
            if let Some(response) = response {
                serde_json::to_writer(&mut stdout, &response)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }

    /// Dispatches one message. `None` means "notification: do not reply".
    pub fn handle(&self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let Some(method) = request.get("method").and_then(|value| value.as_str()) else {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                -32600,
                "invalid request: missing method".into(),
            ));
        };

        id.as_ref()?;
        let id = id.unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => return Some(self.call_tool(id, &params)),
            _ => {
                return Some(error_response(
                    id,
                    -32601,
                    format!("method not found: {method}"),
                ));
            }
        };

        match result {
            Ok(value) => Some(success_response(id, value)),
            Err(err) => Some(error_response(id, -32603, err.to_string())),
        }
    }

    fn initialize(&self, params: &Value) -> Result<Value, MemoryError> {
        let requested = params
            .get("protocolVersion")
            .and_then(|value| value.as_str())
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            DEFAULT_PROTOCOL_VERSION
        };
        Ok(json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "mmux",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Project memory for this workspace: durable meta-docs \
                (decisions, plans, runbooks, notes). Search before assuming context \
                is lost. Only write when the user asks you to remember something.",
        }))
    }

    fn workspace(&self, create: bool) -> Result<Workspace, MemoryError> {
        let resolved = if create {
            self.memory.workspace_for(&self.cwd)
        } else {
            self.memory.workspace_view(&self.cwd)
        };
        resolved.map_err(MemoryError::Other)
    }

    fn call_tool(&self, id: Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(|value| value.as_str()) else {
            return tool_error(id, "tools/call requires a tool name");
        };
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match self.dispatch_tool(name, &args) {
            Ok((text, structured)) => tool_success(id, text, structured),
            Err(err) => tool_error(id, err.to_string()),
        }
    }

    fn dispatch_tool(&self, name: &str, args: &Value) -> Result<(String, Value), MemoryError> {
        match name {
            "memory_search" => self.tool_search(args),
            "memory_read" => self.tool_read(args),
            "memory_write" => self.tool_write(args),
            other => Err(MemoryError::Invalid(format!("unknown tool `{other}`"))),
        }
    }

    fn tool_search(&self, args: &Value) -> Result<(String, Value), MemoryError> {
        let query = string_arg(args, "query").unwrap_or_default();
        let workspace = self.workspace(false)?;
        let filters = super::Filters {
            kind: optional_kind(args)?,
            tag: string_arg(args, "tag"),
            since: string_arg(args, "since"),
        };
        let all = bool_arg(args, "all_workspaces");
        let mut hits = self
            .memory
            .search(Some(&workspace), &query, &filters, all)?;
        let limit = int_arg(args, "limit").unwrap_or(20).clamp(1, MAX_LIMIT);
        hits.truncate(limit);

        let mut text = String::new();
        if hits.is_empty() {
            text.push_str(&format!(
                "No memory documents match{}. Workspace: {}",
                if query.trim().is_empty() {
                    String::new()
                } else {
                    format!(" {query:?}")
                },
                workspace.path.display()
            ));
        } else {
            text.push_str(&format!(
                "{} result{} (workspace {}):\n",
                hits.len(),
                if hits.len() == 1 { "" } else { "s" },
                workspace.path.display()
            ));
            for hit in &hits {
                text.push_str(&format!(
                    "- [{}] {} ({}, updated {})\n  {}\n",
                    hit.kind, hit.title, hit.slug, hit.updated, hit.snippet
                ));
            }
            if text.len() > MAX_SEARCH_TEXT {
                text.truncate(MAX_SEARCH_TEXT);
                text.push_str("\n… truncated; narrow the query with kind/tag/since");
            }
            text.push_str("\nRead one with memory_read(slug).");
        }
        Ok((text, json!({ "hits": hits })))
    }

    fn tool_read(&self, args: &Value) -> Result<(String, Value), MemoryError> {
        let slug = required_string(args, "slug")?;
        let workspace = self.workspace(false)?;
        let doc = self.memory.read(&workspace, &slug)?;
        let text = doc.render();
        let structured = serde_json::to_value(&doc)
            .map_err(|err| MemoryError::Invalid(format!("could not serialize document: {err}")))?;
        Ok((text, structured))
    }

    fn tool_write(&self, args: &Value) -> Result<(String, Value), MemoryError> {
        if self.agent_writes == AgentWrites::Deny {
            return Err(MemoryError::Invalid(
                "agent writes are disabled (memory.agent_writes = deny)".into(),
            ));
        }
        let workspace = self.workspace(true)?;
        let mode = string_arg(args, "mode").unwrap_or_else(|| "create".into());
        let author = super::identity::author(super::Origin::Mcp);

        let doc = match mode.as_str() {
            "create" => {
                let title = required_string(args, "title")?;
                let kind = optional_kind(args)?.unwrap_or(DocKind::Note);
                let body = string_arg(args, "body").unwrap_or_default();
                self.memory.add(
                    &workspace,
                    super::NewDoc {
                        title,
                        kind,
                        tags: string_list(args, "tags"),
                        body,
                        paths: string_list(args, "paths"),
                        supersedes: string_list(args, "supersedes"),
                    },
                    &author,
                )?
            }
            "append" => {
                let slug = required_string(args, "slug")?;
                let body = required_string(args, "body")?;
                self.memory.append(&workspace, &slug, &body, None)?
            }
            "replace" => {
                let slug = required_string(args, "slug")?;
                let body = required_string(args, "body")?;
                self.memory.update(
                    &workspace,
                    &slug,
                    super::Update {
                        title: string_arg(args, "title"),
                        kind: optional_kind(args)?,
                        tags: if args.get("tags").is_some() {
                            Some(string_list(args, "tags"))
                        } else {
                            None
                        },
                        body: Some(body),
                        expected_hash: string_arg(args, "if_hash"),
                    },
                )?
            }
            other => {
                return Err(MemoryError::Invalid(format!(
                    "unknown mode `{other}` (expected create, append, or replace)"
                )));
            }
        };

        let text = format!(
            "Saved {} ({}) in {}: {}",
            doc.slug,
            doc.kind,
            workspace.path.display(),
            doc.title
        );
        let structured = json!({
            "slug": doc.slug,
            "title": doc.title,
            "kind": doc.kind,
            "updated": doc.updated,
            "hash": doc.hash(),
        });
        Ok((text, structured))
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "memory_search",
            "description": "Search this project's memory for durable notes, decisions, plans, and runbooks. Returns compact rows; use memory_read for the full document.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Terms to AND; quote a phrase; prefix a term with - to exclude." },
                    "kind": { "type": "string", "enum": ["note","decision","plan","runbook","retro","reference"] },
                    "tag": { "type": "string" },
                    "since": { "type": "string", "description": "Duration (7d, 12h) or a date (2026-01-01)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "all_workspaces": { "type": "boolean", "description": "Search every known workspace, not just this one." }
                }
            }
        },
        {
            "name": "memory_read",
            "description": "Read one memory document in full, including its provenance and revision timestamp.",
            "inputSchema": {
                "type": "object",
                "properties": { "slug": { "type": "string" } },
                "required": ["slug"]
            }
        },
        {
            "name": "memory_write",
            "description": "Save a durable note, decision, plan, runbook, or retro. Use this only when the user asks you to remember something; never store anything derivable from the code or from git history.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["create","append","replace"], "description": "Default create." },
                    "slug": { "type": "string", "description": "Required for append and replace." },
                    "title": { "type": "string" },
                    "kind": { "type": "string", "enum": ["note","decision","plan","runbook","retro","reference"] },
                    "body": { "type": "string", "description": "Markdown body." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Repository paths this document refers to; used for staleness checks." },
                    "supersedes": { "type": "array", "items": { "type": "string" }, "description": "Slugs this document replaces." },
                    "if_hash": { "type": "string", "description": "Optimistic concurrency for replace." }
                }
            }
        }
    ])
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_success(id: Value, text: String, structured: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
            "isError": false,
        }
    })
}

fn tool_error(id: Value, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": message.into() }],
            "isError": true,
        }
    })
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn required_string(args: &Value, key: &str) -> Result<String, MemoryError> {
    string_arg(args, key).ok_or_else(|| MemoryError::Invalid(format!("`{key}` is required")))
}

fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| item.trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn int_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn optional_kind(args: &Value) -> Result<Option<DocKind>, MemoryError> {
    match string_arg(args, "kind") {
        Some(kind) => DocKind::parse(&kind)
            .map(Some)
            .map_err(|err| MemoryError::Invalid(err.to_string())),
        None => Ok(None),
    }
}

/// Validates a requested slug before any file access (defense in depth: the
/// vault validates too).
pub fn validate_slug(slug: &str) -> Result<(), MemoryError> {
    if is_valid_slug(slug) {
        Ok(())
    } else {
        Err(MemoryError::Invalid(format!("invalid slug `{slug}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(dir: &std::path::Path) -> Server {
        Server::new(
            Memory::new(dir.join("memory")),
            AgentWrites::Explicit,
            dir.to_path_buf(),
        )
    }

    fn call(server: &Server, id: i64, name: &str, arguments: Value) -> Value {
        server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .unwrap()
    }

    #[test]
    fn completes_the_handshake() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let reply = server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }))
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "mmux");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        let unknown = server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": { "protocolVersion": "invalid" }
            }))
            .unwrap();
        assert_eq!(
            unknown["result"]["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );

        assert!(
            server
                .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_none()
        );
    }

    #[test]
    fn lists_tools_with_schemas() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let reply = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["memory_search", "memory_read", "memory_write"]);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn writes_then_reads_a_document() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let written = call(
            &server,
            1,
            "memory_write",
            json!({ "title": "Restart policy", "kind": "decision", "body": "panes are relaunched" }),
        );
        assert_eq!(written["result"]["isError"], false, "{written}");
        let slug = written["result"]["structuredContent"]["slug"]
            .as_str()
            .unwrap();
        assert_eq!(slug, "restart-policy");

        let read = call(&server, 2, "memory_read", json!({ "slug": slug }));
        assert_eq!(read["result"]["isError"], false);
        assert!(
            read["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("relaunched")
        );

        let found = call(
            &server,
            3,
            "memory_search",
            json!({ "query": "relaunched" }),
        );
        assert_eq!(
            found["result"]["structuredContent"]["hits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            found["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Restart policy")
        );
    }

    #[test]
    fn reports_tool_errors_without_breaking_the_protocol() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let missing = call(&server, 1, "memory_read", json!({ "slug": "nope" }));
        assert_eq!(missing["result"]["isError"], true);
        assert!(
            missing["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("nope")
        );

        let bad = call(&server, 2, "memory_write", json!({ "kind": "poem" }));
        assert_eq!(bad["result"]["isError"], true);

        let unknown = call(&server, 3, "nope", json!({}));
        assert_eq!(unknown["result"]["isError"], true);
    }

    #[test]
    fn deny_blocks_agent_writes_but_not_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut memory_server = server(dir.path());
        memory_server.agent_writes = AgentWrites::Deny;
        let reply = call(
            &memory_server,
            1,
            "memory_write",
            json!({ "title": "x", "body": "y" }),
        );
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("disabled")
        );
        let read = call(&memory_server, 2, "memory_search", json!({ "query": "" }));
        assert_eq!(read["result"]["isError"], false);
    }

    #[test]
    fn bad_requests_are_json_rpc_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let unknown = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 9, "method": "resources/list" }))
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);

        let malformed = server
            .handle(&json!({ "jsonrpc": "2.0", "id": 10 }))
            .unwrap();
        assert_eq!(malformed["error"]["code"], -32600);
    }

    #[test]
    fn unicode_titles_still_get_safe_slugs() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = server(dir.path());
        let written = call(
            &server,
            1,
            "memory_write",
            json!({ "title": "日本語", "body": "x" }),
        );
        let slug = written["result"]["structuredContent"]["slug"]
            .as_str()
            .unwrap();
        assert!(is_valid_slug(slug));
        assert_eq!(slug, "doc");
    }
}
