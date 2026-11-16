# Session memory for mmux

Status: design proposal, v2. No code yet.

## 1. What "memory" means here

mmux is the only component that sees the whole session: the project, its
working directory, every pane command, the pi session IDs it resolves, and the
raw PTY byte stream. It should own memory — but the useful thing to own is not
a model-written summary.

Research on agent memory (VibeMemBench, MemDev-Bench, Claude Code's auto-memory
tracker, `claude-mem` field reports) converges on uncomfortable findings:

- Most memory systems do not beat a memory-off baseline on coding tasks.
- The dominant failure is *unrelated history injected into context*, not
  missing recall. 69% of failures are "form degradation", ~1% ranking misses.
- Always-loaded memory rots: "months-old MEMORY.md silently loads into every
  session — no freshness indication, no expiry".
- What survives is boring: append-only source data, a small curated index,
  explicit on-demand retrieval, audit-able provenance.

So: one plaintext layer, and the agent fetches on demand.

| Layer | Contents | Written by | Purpose |
| --- | --- | --- | --- |
| **Docs** (memory proper) | meta-docs: decisions, plans, runbooks, notes, retros | a human, or an agent *when asked* | durable knowledge |

Closing a project never destroys it: the project definition moves to the
**project archive** (see §5.4.1) and memory is keyed by workspace path, so it
survives archive, restore, and even permanent deletion of the project.

Nothing auto-summarizes. Nothing is written into a project repository. Nothing
is injected into a prompt beyond one server-description line.

Explicit non-goals: SQLite or any database, embeddings, vector search,
knowledge graphs, background LLM distillation, screen replay into live panes,
and writing `AGENTS.md`/`CLAUDE.md`/`.pi/mcp.json` into repos.

## 1.1 Implementation status

Landed and covered by tests:

- **Phase 1 — vault + MCP.** `src/memory/{doc,vault,ops,search,identity,readiness,mcp,cli}.rs`.
  `mmux mem …` (add/edit/append/rm/list/show/search/history/status) and
  `mmux mcp` (three tools). `[memory]` config with validation. Read-only
  startup probe of pi's `mcp.json`. HTTP endpoints for docs, search, and status.
  The memory list lives in the project side panel; only new/edit use a modal.
- **Phase 2a — project archive.** `persistence::{archive,load_archive,restore_archived,delete_archived}`,
  `SessionStore::{archive_project,archived_projects,restore_archived,delete_archived}`,
  `/api/archive` endpoints, and the `[a]` panel with typed-confirmation deletion.
- **Phase 2b — removed by decision.** Session/transcript search was dropped:
  pi already ships session browsing (`--resume`, `/resume`), and duplicating it
  in mmux added a second, staler view of the same data.

Not implemented, and not pretended otherwise:

- `mmux mem prune`, `mmux mem stale`, `supersedes` filtering, `INDEX.md`
  generation, the generated pi skill, and the deterministic digest skeleton.

## 2. Principles

1. **Plaintext all the way.** Markdown docs, text logs, JSONL that pi already
   writes. No `*.db`, no binary blobs, no migrations, no schema in the critical
   path. Search is a streaming scan; `rg` over the vault is a valid debugging
   tool.
2. **No derived state.** No index to rebuild, therefore no index to corrupt or
   to disagree with the files.
3. **Deliberate writes.** No background distillation. A fact is kept because
   someone saved it (`mmux mem add`) or asked an agent to.
4. **Bounded always-on cost.** The only thing an agent sees before asking is a
   one-line MCP server description. Everything else is a tool call.
5. **Provenance on every doc.** author, created/updated, source session and
   pane, revision history. Stale docs look stale.
6. **Scope by canonical path, never by name.** Workspace key is
   `basename + short hash of canonicalize(cwd)`; two `~/src/api` trees never
   share memory.
7. **Configuration is TOML, not environment.** Every knob lives in
   `~/.config/mmux.toml` and is printed by `--print-default-config`. The
   systemd unit therefore sets no mmux `Environment=` lines. `RUST_LOG` stays as
   a debugging override, and `MMUX_CONFIG` only *selects a file*.
8. **No network, no repo writes.** Vault under `$XDG_STATE_HOME/mmux`, mode
   `0700`; the MCP server is stdio-only and inherits the pane's environment.
9. **Deletion is never the default.** Closing a project archives it; panes stop
   but the definition, and all memory for its workspace, survive. Irreversible
   deletion is a deliberate action in the archive panel.

## 3. Layout

```text
$XDG_STATE_HOME/mmux/
  projects/                      # live projects, unchanged format
    index.json                   # live project IDs in sidebar order
    <project-id>/project.json
  archive/                       # closed projects, same format + metadata
    index.json                   # archived project IDs, newest first
    <project-id>/
      project.json               # byte-identical format to projects/
      archived.json              # {version, archived_at, original_position, pane_count}
  memory/
    workspaces.json              # canonical path -> workspace key
    workspaces/
      <basename>-<hash8>/
        workspace.json           # {version, path, created, last_seen}
        docs/
          <doc-slug>.md          # TOML frontmatter + markdown body
        _history/<doc-slug>/<utc-ns>.md
        INDEX.md                 # optional, generated, for humans
```

Memory is keyed by workspace path, not project id, so closing and reopening a
project keeps its memory, and two projects on one directory share it. Archiving
moves a `projects/<id>/` directory to `archive/<id>/`; it never touches
`memory/`.

## 4. Doc format

```markdown
+++
title = "Why panes restart the same command"
slug = "pane-restart-policy"
kind = "decision"          # note | decision | plan | runbook | retro | reference
tags = ["session", "restart"]
created = "2026-11-16T09:12:03Z"
updated = "2026-11-16T09:12:03Z"
author = "agent:pi"        # or "user"
source = { session = "01a1e1ab-…", pane = "bottom-left" }
paths = ["src/session/pane.rs"]   # optional, staleness checks
supersedes = []                   # optional, slugs this replaces
+++

Runners are relaunched because a PTY cannot be serialized; the project
definition, not the process, is the durable object.
```

TOML frontmatter reuses the existing `toml` dependency. Unknown keys survive a
round-trip so a UI save never damages a hand-edited doc.

### 4.1 Author is inferred, never asked for

No `--author` flag. At write time the author string is derived from the caller's
environment and frozen into the doc; later edits never rewrite it.

| Signal | `author` | `source` |
| --- | --- | --- |
| `MMUX_AGENT=pi` (mmux sets it for panes whose command is `pi`) | `agent:pi` | `{session, pane}` from `MMUX_SESSION_ID`/`MMUX_PANE_SLOT` |
| `PI_CODING_AGENT=true` without `MMUX_AGENT` (nested pi, pi outside mmux) | `agent:pi` | `{session: null, pane: null}` |
| HTTP UI | `user` | `{ui: true}` |
| `mmux mem` from a shell with no agent env | `cli:<unix user>` | `{cwd}` |

Two accuracy notes, both verified against pi 1.5.1:

- pi exports `PI_CODING_AGENT=true` into its own process environment, so MCP
  children inherit it.
- pi **deletes** `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`
  and `PI_REASONING_LEVEL` from inherited environments and re-adds them only for
  shell tool commands. An MCP server therefore cannot learn the session or the
  model from `PI_*`; it uses `MMUX_*` instead, and the history layer can backfill
  `source.model` later from the session JSONL. Never block a write on it.

`author` is provenance, not authorization. Nothing is gated on it.

## 5. Interface: MCP first

### 5.1 Why MCP instead of a CLI or an injected prompt

- **Portable.** One protocol works for pi, Claude Code, Codex, and anything
  else with MCP. mmux hosts arbitrary commands in panes, so a pi-only
  mechanism would reach only pi panes.
- **Zero always-on context.** pi supports `exposure: "deferred"` (tools are not
  declared until `tool_search` loads them) and per-tool `toolExposure`. Only the
  two hot tools are `direct`; the server's one-line `description` is the whole
  fixed cost.
- **Deliberate retrieval is a feature.** Research says agent-decided timing
  beats passive injection ("a memory agent that decides when to speak
  outperforms passive bank exposure, always-on injection, advisor-only guidance,
  general retrieval": +8.3 pts Terminal-Bench 2.0). MCP has no auto-inject, and
  that is the behaviour we want.
- **Cheap to build.** A stdio MCP server is newline-delimited JSON-RPC:
  `initialize`, `notifications/initialized`, `tools/list`, `tools/call`,
  `ping`. ~300 lines with `serde_json`, no new dependency.

### 5.2 Configuration: `[memory]` in `mmux.toml`

The MCP server has no configuration of its own. `mmux mcp` is the same binary,
so it loads the same `~/.config/mmux.toml` (or the file named by `--config` /
`MMUX_CONFIG`) and reads `[memory]`:

```toml
[memory]
# Master switch for the vault and the MCP server.
enabled = true
# Vault root. Defaults to $XDG_STATE_HOME/mmux/memory.
# dir = "~/.local/state/mmux/memory"
# Largest document the CLI, UI, or MCP will write.
max_doc_bytes = 65536
# Previous revisions kept per document.
history_versions = 20
# "explicit": agents may write when asked; "deny": read-only for agents.
agent_writes = "explicit"
# pi integration for the readiness probe. mmux reads this, never writes it.
pi_agent_dir = "~/.pi/agent"
```

[`config.example.toml`](../config.example.toml) carries the same keys, so
`mmux --print-default-config` is the authoritative reference.

**The workspace is resolved from the working directory, not from env.**
`mmux mcp` and `mmux mem` canonicalize `cwd`, then pick the *longest known
workspace path that is a prefix* of it (`workspaces.json`), falling back to a
fresh key derived from `cwd`. So a doc written from a subdirectory, a shell
pane, or a non-mmux terminal lands in the expected vault, and `cd` inside a
project never creates a stray one.

The only environment mmux adds to panes is **identity, not configuration**
(like `TMUX_PANE`), and everything is optional with a fallback:

```text
MMUX_PROJECT_ID, MMUX_PROJECT_NAME   # informational
MMUX_PANE_SLOT, MMUX_SESSION_ID      # provenance: source.pane / source.session
MMUX_AGENT=pi                        # provenance: author=agent:pi
MMUX_CONFIG                          # path only, and only when mmux was
                                     # started with an explicit config file
```

These are injected at spawn (into `PaneSpec.env`, never persisted into
`project.json`). Missing values degrade gracefully: the author falls back to
`cli:<user>` or `agent:pi` via inherited `PI_CODING_AGENT`, and the workspace
still resolves from `cwd`. Nothing here is user-tunable, so nothing here
belongs in the TOML.

pi spawns stdio MCP servers with the session's environment, so **one** global
entry in `~/.pi/agent/mcp.json` works for every pane and every project:

```json
{
  "mcpServers": {
    "mmux": {
      "command": "mmux",
      "args": ["mcp"],
      "description": "Project memory: search/read/write durable meta-docs and past session history",
      "exposure": "deferred",
      "toolExposure": {
        "memory_search": "direct",
        "memory_read": "direct",
        "memory_write": "deferred",
        "memory_delete": "hidden"
      },
      "timeout": 20
    }
  }
}
```

No per-project config file, no repo writes, no trust prompt. The server resolves
its workspace from `cwd` plus `[memory]` in mmux.toml, so it behaves the same
whether pi was started by mmux or by hand.

**mmux never writes this file.** It is user configuration, shared with every
pi session on the machine. mmux's job is to notice when it is absent or broken
and say so once.

### 5.2.1 Configuration resolution, precedence, and validation

One rule: **if it is a setting, it is a key in `mmux.toml`.** Panes never carry
settings, the systemd unit never carries settings, and the MCP entry never
carries settings. `[logging]` already follows this rule (its filter moved into
TOML so the unit could drop `Environment=RUST_LOG`); `[memory]` is the second.

Resolution order, first match wins:

1. `--config PATH`, on `mmux` or `mmux mcp`.
2. `MMUX_CONFIG` — a *path*, not a setting. mmux sets it on panes only when it
   was itself started with a non-default file, so `mmux mcp` reads the same
   vault the server does.
3. `~/.config/mmux.toml`.
4. Built-in defaults, which are exactly what `mmux --print-default-config`
   prints.

No `[memory]` value is read from the environment. The only environment mmux
honours at all are the pre-existing `MMUX_CONFIG` (file selector) and
`MMUX_BIND` (listen-address override), both of which already have flags, plus
`RUST_LOG`, which overrides `[logging].filter` for one run as a debugging aid.
Everything else is a TOML key.

| Key | Type | Default | Validation |
| --- | --- | --- | --- |
| `memory.enabled` | bool | `true` | — |
| `memory.dir` | path | `$XDG_STATE_HOME/mmux/memory` | `~` expanded; must be absolute; created `0700` |
| `memory.max_doc_bytes` | int | `65536` | `1024..=4194304` |
| `memory.history_versions` | int | `20` | `0..=100`; `0` disables history |
| `memory.agent_writes` | enum | `"explicit"` | `explicit` \| `deny` |
| `memory.pi_agent_dir` | path | `~/.pi/agent` | read-only; never created or written by mmux |

Deliberately **not** configuration, and therefore not keys:

- pane identity (`MMUX_PANE_SLOT`, `MMUX_SESSION_ID`, `MMUX_AGENT`) — process
  context, like `TMUX_PANE`;
- the workspace key — derived from `cwd` plus `workspaces.json`;
- pi's `mcp.json` — user-owned, read-only to mmux, probed and reported only;
- the MCP tool catalogue and its exposure — shipped with the binary, so the
  behaviour of `memory_*` is a property of the mmux version, not a local knob.

Staging requirement: `Config` uses `deny_unknown_fields`, so a `[memory]` block
in `config.example.toml` fails to parse until `MemoryConfig` exists. The struct,
its `Default`, the example block, and the `example_config_matches_defaults` test
must land in one commit, and `--print-default-config` output must round-trip.
Add one test per validation bound plus one asserting an unknown `[memory]` key
is rejected.

### 5.2.2 Startup readiness probe

`mmux` runs one read-only probe at startup, before serving, and caches the
verdict in `AppState`:

1. Is `pi` on `PATH`? If not, status `NotApplicable` — no warning, no banner.
   (mmux is useful without pi; don't nag.)
2. Read `[memory] pi_agent_dir` from mmux.toml (default `~/.pi/agent`).
3. Read `<config>/mcp.json`. A parse error is reported verbatim as `Invalid`.
4. Find the entry: prefer the server named `mmux`; accept any entry whose
   `command` basename is `mmux` and whose `args` contains `mcp`; report duplicate
   matches as `Ambiguous`.
5. Validate it: command resolves on `PATH` (or is an existing path), args
   contain `mcp`, `enabled` is not `false`, and `exposure` is `deferred` or
   `direct` (a `codemode`/`hidden` server would make memory unreachable without
   `tool_search`, so that is a warning, not silent).

Verdicts: `Ready`, `NotApplicable`, `Missing`, `Disabled`, `Mismatched(reason)`,
`Invalid(error)`, `Ambiguous`. The probe never writes, never repairs, and never
fail-closes: memory still works from the CLI and UI when the verdict is bad.

On anything other than `Ready`/`NotApplicable`, mmux logs one `WARN` at startup
and exposes the same text through `GET /api/memory/status`, which the UI renders
as a dismissible banner. Both show copy-paste steps:

```text
pi memory server not configured: <verdict detail>
Add this to <config>/mcp.json under "mcpServers" (mmux will not do it for you):

  "mmux": {
    "command": "mmux",
    "args": ["mcp"],
    "exposure": "deferred",
    "toolExposure": {
      "memory_search": "direct",
      "memory_read": "direct",
      "memory_write": "deferred",
      "memory_delete": "hidden"
    }
  }

then restart pi panes, or run: pi mcp add mmux --exposure deferred -- mmux mcp
```

`mmux mem status` prints the same verdict, and `mmux mem status --refresh`
re-runs the probe (also the banner's "re-check" button) so a user who edits
`mcp.json` does not have to restart mmux. There is no polling.

A project-level `.pi/mcp.json` can override or disable the entry. The probe
reports what the user-level file says and the message mentions that a project
file may still win; mmux does not read repos to guess.

Per-pane gating uses pi's existing flags, not new code:
`--exclude-tools mcp__mmux__memory_write` for a read-only reviewer pane, or
`--tools …` allowlists.

### 5.3 Tool catalog

Few tools, each answering one question. All return compact text plus
`structuredContent`.

| Tool | Exposure | Purpose |
| --- | --- | --- |
| `memory_search(query, kind?, tag?, workspace?, since?, limit?)` | direct | ranked index rows: slug, title, kind, updated, one-line snippet (~50–100 tokens/hit) |
| `memory_read(slug, max_bytes?)` | direct | full doc with frontmatter and provenance |
| `memory_write(title, kind, body, tags?, paths?, supersedes?, mode?"create"\|"append"\|"replace")` | deferred | save a doc; returns slug + hash |
| `memory_delete` | hidden | CLI/UI only; unattended deletion is a footgun |
| `memory_list` | — | folded into `memory_search` with an empty query |

`memory_write`'s description tells the model to use it only when the user asks
to remember something. pi runs every MCP call through its tool pipeline, so
users who want a hard gate add a `tool_call` handler; mmux additionally supports
`[memory] agent_writes = "explicit" | "deny"`.

Resources are optional later: `mmux://<workspace>/docs/<slug>` for clients that
browse resources. Not needed for pi.

### 5.4 CLI and HTTP/UI stay

The MCP server is a thin adapter over `mmux::memory` operations. The same
operations back:

- **`mmux mem`** (subcommand, no server, no auth) for humans and shell scripts:
  `add/edit/append/rm/list/show/search/history/stale/reindex/prune`.
  `--json` for anything that wants structure.
- **HTTP** for the browser: `GET /api/memory/docs`, `GET/POST/PUT/DELETE
  /api/memory/docs/{slug}` (`If-Match` on content hash → 409), and
  `GET /api/memory/search`. Authenticated and CSRF-gated like every other
  mutation.
- **UI**: a sidebar drawer listing docs by kind, a search box, a plain-text
  editor, and a conflict banner. Markdown renders with a small allowlist —
  no raw HTML, matching the current CSP.

### 5.4.1 Project archive (the `[a]` panel)

Closing a project must not be able to lose knowledge. Today
`SessionStore::close_project` terminates panes, rewrites the index, and calls
`persistence::delete_project` — an irreversible `rm -rf` of
`projects/<storage-id>/`. The plan replaces that with a two-stage flow:

1. The sidebar **×** (tooltip becomes “Archive project”) and
   `DELETE /api/projects/{id}` stop the panes and **move** the directory:
   `projects/<storage-id>/` → `archive/<storage-id>/`, plus an `archived.json`
   sidecar. `project.json` is copied byte-for-byte, so `archive/` and
   `projects/` share one `ProjectFile` version and restore is a move back.
2. Permanent deletion only happens from the archive panel, behind a typed
   confirmation.

Memory is untouched by both steps: it is keyed by workspace path, so the docs
and history of an archived project stay searchable and remain visible in the
memory drawer, which lists every known workspace, archived projects included.

**UI.** A text button labelled `[a]` sits immediately to the right of
`+ new project`, in the same row:

```html
<div class="sidebar-actions">
  <button id="new-project" class="new-project" type="button">+ new project</button>
  <button id="archive-toggle" class="archive-toggle" type="button"
          aria-label="Archived projects" title="Archived projects">[a]</button>
</div>
```

Its `title` gains the count (`Archived projects (3)`) when non-zero. It opens a
`<dialog>` listing entries newest-first: name, working directory (middle-elided),
archived date, pane count, and — when `[memory].enabled` — document count for
that workspace path. Each row has **Restore** and **Delete permanently**.

The panel has three states: populated, `No archived projects`, and
**unreadable** (the index failed to parse), which shows the path and the error
instead of an empty list, so “archive lost” is never indistinguishable from
“archive empty”.

**HTTP.** Archive identity is the durable `Uuid` (the runtime `ProjectId` is a
`u64` that restarts at 1 and is meaningless after a restart):

```text
GET    /api/archive                 -> [{id: uuid, name, cwd, archived_at,
                                         pane_count, memory_docs}]
POST   /api/archive/{uuid}/restore  -> ProjectView (201); 404 unknown; 409 if live
DELETE /api/archive/{uuid}          -> 204; requires X-MMUX; never touches memory
```

`DELETE /api/projects/{id}` keeps its path and `204` status but becomes
“archive”; the change is nameable in the UI and in the log
(`archived project <uuid>` rather than `closed project`).

**Restore.** Move `archive/<uuid>/` back to `projects/<uuid>/`, drop
`archived.json`, spawn panes from the saved definitions exactly like startup
restore (a missing cwd or command is reported by the pane and retried, not a
reason to refuse), then append to the end of the sidebar. The storage UUID is
reused, so nothing keyed to it changes. Restoring is refused with 409 if that
UUID is already live.

**Permanent delete.** Removes `archive/<uuid>/` and its index entry, and says so
in the confirmation text. It does **not** delete workspace memory: another live
or archived project may share that path, and memory outliving projects is the
point. When no project references the path, the panel offers a separate
**Forget workspace memory** action that requires typing the absolute path, and
that action lives with the memory drawer, not with project deletion.

**Failure modes.** A failed move is rolled back (the directory stays in
`projects/` or `archive/`, whichever existed, and the index is rewritten from
disk). A corrupt `archive/index.json` marks archive mutations untrusted — the
rest of mmux, including live projects and all memory reads, keeps working — the
same trust rule the live index already uses. Archive entries are never expired
automatically; the panel reports total size and deletion is always manual.

## 6. Search over plaintext

No index, so the algorithm must be good enough to not miss one:

1. **Scope first.** Resolve candidate files from the workspace dir; apply
   `--since` by mtime and `kind`/`tag` by a cheap frontmatter scan. Never read
   bytes you can exclude from a `stat`.
2. **Stream.** 64 KiB chunks, `memchr`-style literal search (`std` `find` is
   adequate; add `memchr` only if benchmarks demand it). Lines longer than
   8 KiB are truncated. Cap total scanned bytes (`--max-bytes`, default 256 MiB)
   and report `truncated: true` rather than pretending completeness.
3. **Match.** Case-insensitive; whitespace-separated terms are AND; quoted
   strings are phrases; a leading `-` excludes.
4. **Rank.** title hit ×10, tag ×6, phrase ×4, term coverage ×1, doc-kind
   weight, recency boost.
5. **Snippet.** ±80 chars around the first hit, whitespace-collapsed, control
   characters stripped.
6. **Bounds.** Stop at `limit` (default 20, max 200) and a 2 s budget. Long
   scans run on a blocking pool with cancellation so a UI search cannot stall
   the async runtime.

Docs are a few hundred KB, so search is effectively instant. If that ever stops
being true, the answer is narrower scope — not a database.

## 7. Session history: removed

An earlier draft indexed pi's session JSONL and captured terminal scrollback.
Both were dropped: pi already provides session browsing (`--resume`, `/resume`,
`--session`), and a second copy inside mmux would be staler than the source.
Pi sessions are still readable by the user and by pi itself; mmux no longer
reads or indexes them.

Document revisions (`mmux mem history`) are unrelated and remain.

## 8. Phases

### Phase 1 — Vault + MCP (the whole memory proper)

- `src/memory/{mod,vault,doc,search,ops}.rs` — workspace keys, slugs, atomic
  write (`tmp` + `rename`), history cap, frontmatter round-trip, search, ops.
- `src/memory/mcp.rs` — stdio JSON-RPC server (`mmux mcp`): `initialize`
  returning `{protocolVersion, capabilities: {tools: {}}, serverInfo: {name,
  version}, instructions}` (pi validates `serverInfo.name` and `.version` as
  strings, and rejects a protocol version outside its supported list, which
  includes 2025-11-25 and 2025-06-18), `tools/list`, `tools/call`, `ping`,
  `notifications/*` ignored gracefully, `isError` results for user errors,
  stderr logs only (stdout is the protocol).
- `src/main.rs`: `mem` and `mcp` subcommands run before any PAM/server setup;
  the readiness probe runs once in `run()` and is stored in `AppState`.
- `src/memory/readiness.rs`: the probe (read-only), verdict enum, and the
  human-readable remediation text used by the log, the API, and
  `mmux mem status`. Never writes to the pi config dir.
- `src/config.rs`: `[memory]` section in one commit with `MemoryConfig`, its
  `Default`, the `config.example.toml` block, and the validation tests, per the
  staging rule in §5.2.1. `[logging]` already set this precedent: it moved the
  unit's `Environment=RUST_LOG` into the TOML, and `[memory]` follows the same
  rule — no new environment variables, and no memory key read from the unit.
- `src/session/mod.rs::spawn_project`: inject identity env into each `PaneSpec`
  (`MMUX_PROJECT_ID`, `MMUX_PANE_SLOT`, `MMUX_SESSION_ID`, `MMUX_AGENT`, and
  `MMUX_CONFIG` only when mmux itself used a non-default file). No settings, and
  never persisted into `project.json`.
- `src/web/{api,mod}.rs` + `web/app.js`: doc CRUD, search, editor drawer.
- `config.example.toml`, README, `mmux.service` comment, `docs/` updated.

Exit criteria: an agent in a pi pane discovers `memory_*` via `tool_search`,
writes a doc when asked, and a second pane reads it back; a doc written by the
CLI is visible in the UI without a restart; deleting every file except
`docs/*.md` loses nothing; with the entry removed from `mcp.json` the startup
`WARN` and the UI banner appear, and with it restored `mmux mem status` reports
`Ready` without restarting mmux; `mmux --print-default-config` includes
`[memory]` and the output round-trips through `Config::from_toml`; `grep -r
"MMUX_" mmux.service` finds nothing.

### Phase 2 — Project archive + session history

**2a. Project archive** (small, and it is what makes “delete never loses
memory” true):

- `persistence`: `archive()`, `restore()`, `delete_archived()`, `load_archive()`,
  with `archive/index.json` and the `archived.json` sidecar; live index untouched
  by archive failures.
- `SessionStore::close_project` becomes `archive_project` (terminate panes, move
  the directory, keep the storage UUID); a `restore_archived` path reuses the
  existing startup restore so pane specs and session IDs resolve identically.
- `DELETE /api/projects/{id}` → archive; `GET /api/archive`,
  `POST /api/archive/{uuid}/restore`, `DELETE /api/archive/{uuid}`.
- `web/index.html` + `app.js` + `style.css`: the `[a]` button next to
  `+ new project`, the archive dialog, its populated/empty/unreadable states,
  and Restore / Delete permanently with typed confirmation.
- README: closing now archives; document the `[a]` panel, restore, and
  permanent deletion, and the fact that memory outlives both.

Exit criteria: closing a project makes it appear under `[a]` with its memory doc
count, and the entry survives a mmux restart; Restore brings back the same name,
cwd, pane definitions, session IDs, and memory; Delete permanently removes only
`archive/<uuid>/` and leaves `memory/` byte-identical; a corrupt
`archive/index.json` shows the unreadable state while live projects and memory
keep working.

### Phase 3 — Polish

- `supersedes` links, `stale` check on `paths:`, `verified_at`.
- Generated `INDEX.md` (≤ 40 lines) for humans and non-MCP agents.
- Optional `mmux mem digest` that assembles *facts* into a markdown skeleton for
  the agent to refine and save deliberately. Still no background model calls.

## 9. Risks

| Risk | Mitigation |
| --- | --- |
| MCP tool bloat / prompt cost | `deferred` + two `direct` tools; one-line server description; `toolExposure` patterns |
| Agent writes garbage memory | `memory_write` only on request; `agent_writes = "explicit"`; UI shows author + hash; history is revertible |
| Stale or wrong `mcp.json` entry | read-only probe covers path drift, `enabled: false`, wrong exposure, and JSON errors; remediation text is copy-paste, and `status --refresh` re-checks |
| Stale doc entries | 40-line index, `stale`, `supersedes`, `updated` shown in every hit |
| Scan cost growth | mtime scoping and a per-query byte/time cap; no index by design |
| MCP protocol drift | echo a version from pi's supported list; keep the server stateless and schema-minimal |
| Workspace identity churn | stored absolute path + `mmux mem rekey` |
| Injected args break user commands | none are injected: env vars only, and they are never persisted into `project.json` |
| Archived project deleted by mistake | perma-delete only from the `[a]` panel with typed confirmation; memory is a separate filesystem tree and is never touched by it |
| Archive grows without bound | no automatic expiry by design; the panel reports total size and deletion is manual |

## 10. Verification

- Unit: frontmatter round-trip incl. unknown keys; slug collision; atomic write
  under simulated ENOSPC; history cap; search ranking fixtures (title > tag >
  phrase > term); phrase/exclusion parsing; truncation flags.
- Config: every `[memory]` bound in §5.2.1 rejected outside its range; unknown
  `[memory]` key rejected (`deny_unknown_fields`); `config.example.toml` equals
  `Config::default()`; `mmux --print-default-config` round-trips; `MMUX_CONFIG`
  and `--config` select the same file in the server and in `mmux mcp`; no
  memory setting is read from the environment.
- MCP: a scripted stdio client (test harness in `tests/memory_mcp.rs`) exercises
  `initialize` → `tools/list` → `tools/call` → unknown method → malformed JSON →
  oversized argument; asserts stdout is protocol-only and `isError` is set for
  user errors.
- CLI/integration: add/search/edit/rm against a temp state dir; concurrent CLI
  write + server read; `If-Match` 409; slug traversal (`../`, absolute,
  unicode) rejected.
- Web: auth required, `X-MMUX` required, JSON bodies only.
- Archive: close → entry in `GET /api/archive` + on disk under `archive/`; restart
  keeps it; restore reuses the same UUID and resolves session IDs; restore of a
  live UUID is 409; perma-delete leaves `memory/` byte-identical (compare a
  recursive hash before/after); failed move rolls back; corrupt archive index
  disables archive mutations only; `[a]` renders the unreadable state; delete
  requires typed confirmation in the UI and `X-MMUX` on the wire.
- Perf: 10k docs search < 20 ms.
- Manual: `cargo fmt && cargo clippy --all-targets && cargo test`; two browsers
  editing one doc; `kill -9` mid-write; `pi mcp list` shows `mmux` connected with
  the expected tools.

## 11. Decisions and open questions

Decided: plaintext only, no database, no derived index; docs written on demand;
memory scoped by canonical workspace path; MCP as the only agent interface
(no extension, no injected system prompt); stdio transport; `memory_delete` not
exposed to agents; no repo writes.

Decided: mmux never modifies pi configuration. It probes readiness once at
startup, warns once, and publishes the verdict plus copy-paste remediation via
`/api/memory/status`, the UI banner, and `mmux mem status --refresh`.

Decided: `author` is inferred from `MMUX_*`/`PI_CODING_AGENT` at write time and
frozen; it is provenance, not authorization.

Decided: session/transcript history is out of scope. pi owns session browsing,
so mmux keeps only documents and never indexes or copies transcripts.

Decided: closing a project archives it. `projects/<id>/` moves to
`archive/<id>/`, panes stop, the storage UUID is reused on restore, and nothing
is ever `rm -rf`ed from the sidebar. Permanent deletion lives only in the `[a]`
panel and never removes workspace memory, because memory is keyed by path and
must be able to outlive the project that created it.

Decided: configuration lives in `mmux.toml` only. `[logging]` moved there
first (the systemd unit no longer sets `RUST_LOG`); `[memory]` follows the same
rule. `mmux mcp` loads the file itself and resolves its workspace from `cwd`,
so no memory setting is passed through the environment. The unit sets no mmux
environment variables; `RUST_LOG` and `MMUX_CONFIG` are a debugging override and
a file selector, not configuration.

Open: whether the readiness probe should also detect a *stale* vs *absent* entry
as different severities in the UI (currently one warning level), and whether the
banner should be suppressed for users who deliberately use another MCP client.
