# Session memory for mmux

Status: design proposal. No code yet.

## 1. What "memory" means here

mmux is the only component in the stack that sees *all* of a session: the
project, its working directory, every pane command, the pi session IDs it
resolves, and the raw PTY byte stream. That makes it the natural owner of
memory — but the useful thing to own is not a model-written summary.

Research on agent memory (VibeMemBench, MemDev-Bench, Claude Code auto-memory
issue tracker, `claude-mem` field reports) converges on a few uncomfortable
findings:

- Most memory systems do not beat a memory-off baseline on coding tasks.
- The dominant failure is *unrelated history injected into context*, not
  missing recall. 69% of failures are "form degradation"; ~1% are ranking misses.
- Always-loaded memory rots: "months-old MEMORY.md silently loads into every
  session — no freshness indication, no expiry".
- The patterns that do survive are boring: append-only source data, a small
  curated index, explicit on-demand retrieval, and provenance you can audit.

So the design here has two layers with sharply different jobs:

| Layer | Contents | Written by | Purpose |
|---|---|---|---|
| **Docs** (memory proper) | meta-docs: decisions, plans, runbooks, project notes, retros | a human, or an agent *when asked* | durable knowledge |
| **Archive** (history) | terminal transcripts + pi session transcripts | mmux, automatically | recall and search |

Docs are the source of truth and are never auto-generated. The archive is
read-only evidence that makes recall possible; it is never injected into a
prompt by default. Nothing in mmux writes into a project repository.

Explicit non-goals: auto-summarization, embeddings/vector search, knowledge
graphs, screen replay into live panes, and writing `AGENTS.md`/`CLAUDE.md` into
repos.

## 2. Design principles

1. **Deliberate writes.** No background distillation. If a fact is worth
   keeping, someone saves it (`mmux mem add`) or the user asks an agent to.
2. **Docs are files, the index is derived.** A corrupt or deleted index is
   always rebuildable from the vault. No memory content exists only in SQLite.
3. **Bounded always-on cost.** At most one small generated index file
   (~40 lines, hard 4 KB cap) is ever appended to an agent's context.
4. **Provenance on every doc.** author, created/updated, source session and
   pane, and a revision history. Stale docs are visible as stale.
5. **Scope by canonical path, never by name.** Workspaces are keyed by
   `canonicalize(cwd)` plus a short hash; the bug where two
   `~/src/api` directories share memory does not exist here.
6. **No repo writes, no network.** Vault lives under `$XDG_STATE_HOME/mmux`,
   mode `0700`; the CLI talks to files only.

## 3. Layout

```text
$XDG_STATE_HOME/mmux/
  projects/                      # existing project persistence, unchanged
  memory/
    workspaces.json              # canonical path -> workspace key (human index)
    workspaces/
      <basename>-<hash8>/
        workspace.json           # {version, path, created, last_seen}
        AGENT.md                 # generated: the memory protocol + doc index
        docs/
          <doc-slug>.md          # TOML frontmatter + markdown body
        _history/<doc-slug>/<utc-ns>.md
        sessions.json            # pane -> pi session id manifest (phase 2)
    transcripts/                 # phase 2, derived
      <workspace>/<pane-uuid>.log
    index.db                     # phase 2, derived SQLite FTS5
```

`workspace.json` and `workspaces.json` keep the absolute path, so renames and
moves are detectable (`mmux mem rekey --from … --to …`).

### Doc format

```markdown
+++
title = "Why panes restart the same command"
slug = "pane-restart-policy"
kind = "decision"          # note | decision | plan | runbook | retro | reference
tags = ["session", "restart"]
created = "2026-11-16T09:12:03Z"
updated = "2026-11-16T09:12:03Z"
author = "agent:pi"        # or "user"
source = { session = "01a1e1ab-...", pane = "bottom-left" }
paths = ["src/session/pane.rs"]   # optional: staleness checks
supersedes = []                   # optional: slugs this replaces
+++

Runners are relaunched because a PTY cannot be serialized; the project
definition, not the process, is the durable object.
```

TOML frontmatter (`+++`) reuses the existing `toml` dependency and keeps one
serializer for config and docs. Unknown keys are preserved on round-trip so
hand-edited docs are not damaged by a save from the UI.

## 4. Interfaces

### CLI (same binary, new subcommand)

`mmux mem …` — no server needed, no auth, safe to run from any pane:

```text
mmux mem add --title "…" [--kind] [--tag] [--file -]      # create doc
mmux mem edit <slug> [--file -]                          # replace body
mmux mem append <slug> --file -                          # append section
mmux mem rm <slug> [--yes]
mmux mem list [--kind] [--tag] [--all-workspaces] [--json]
mmux mem show <slug> [--json] [--raw]
mmux mem search <query> [--kind] [--since] [--all-workspaces] [--json]
mmux mem history <slug>
mmux mem stale [--older-than 90d]
mmux mem reindex | status | prune --older-than <dur>
```

Search output is a compact table by default (title, kind, updated, one-line
summary, ~50–100 tokens/result) — deliberately the "layer 1" of progressive
disclosure. `--json` is for agents that want to page through results.

### Pane environment (phase 1)

`spawn_project` already builds a per-pane `env` map. Add:

```text
MMUX_PROJECT_ID, MMUX_PROJECT_NAME, MMUX_PROJECT_CWD
MMUX_WORKSPACE_KEY, MMUX_MEMORY_DIR, MMUX_MEMORY_DOCS
MMUX_MEMORY_CLI="mmux mem"      # so agents don't guess the syntax
MMUX_PANE_SLOT, MMUX_SESSION_ID (when resolved)
```

These are injected at spawn time and are **not** persisted in the pane
command, so moving the state dir or changing config never leaves a stale path
baked into `project.json`.

### Context injection (opt-in, default on for pi)

For panes whose command is `pi` (basename match), mmux appends
`--append-system-prompt <vault>/AGENT.md` at spawn time. The file is small:

```markdown
## Project memory (mmux)
This workspace has a memory vault at <path>. Save durable knowledge with
`mmux mem add …`; search it with `mmux mem search <query>` before assuming
prior context is lost. Do not write memory unless asked.
## Documents
- [decision] Pane restart policy — runners are relaunched, not resurrected
- [plan] Memory v1 — vault first, transcript indexing second
```

Everything else (emacs, shells, custom tools) gets the env vars only; its
authoritative interface is the files under `MMUX_MEMORY_DIR`. Injection is
configurable (`memory.inject = "pi" | "none"`) and capped at 4 KB.

### HTTP + UI (phase 1)

```text
GET    /api/memory/docs?kind=&tag=&q=
GET    /api/memory/docs/{slug}            # content + meta + content-hash
POST   /api/memory/docs                   # create (X-MMUX, JSON body)
PUT    /api/memory/docs/{slug}            # If-Match: <hash> -> 409 on conflict
DELETE /api/memory/docs/{slug}
GET    /api/memory/search?q=&all_workspaces=
```

A right-hand drawer (or a tab in the existing sidebar) lists docs grouped by
kind, with a search box and a plain-text editor. Markdown is rendered as text
blocks with a small allowlist (bold, code, links, lists) — no raw HTML, no new
script surface, consistent with the current CSP.

## 5. Phases

### Phase 1 — Vault (docs) — the memory proper

New module `src/memory/`:

- `vault.rs` — workspace key resolution (`canonicalize` + `sha256[..8]`),
  slug generation and collision handling, atomic write (`write` to
  `<slug>.md.tmp` then `rename`), history capture (cap 20 versions / 90 days),
  frontmatter parse/serialize with unknown-key preservation.
- `doc.rs` — `Doc`, `DocKind`, `DocMeta`, validation (`kind` allowlist, title
  length, doc size ≤ `memory.max_doc_bytes`, default 64 KB).
- `scan.rs` — in-memory index for Phase 1: load all docs at startup,
  reconcile on demand (workspace `docs/` mtime, then per-file
  `(mtime, size)`), tokenized substring match, recency-weighted ordering.
  No new dependency. `MemoryIndex` is a trait so Phase 2 can swap in SQLite.
- `render.rs` — generate `AGENT.md` (index + protocol), hard caps
  (`memory.index_max_lines`, default 40; `index_max_bytes`, 4096), deterministic
  ordering, overflow truncation with an explicit "… N more, use `mmux mem search`".
- `inject.rs` — spawn-time env + pi argument injection (never persisted).

Wire-up:
- `src/config.rs`: new `[memory]` section (`enabled`, `dir`, `max_doc_bytes`,
  `history_versions`, `index_max_lines`, `inject`, `include_in_search`).
  Update `DEFAULT_CONFIG_TOML` and `config.example.toml` (README documents it).
- `src/session/mod.rs::spawn_project`: compute workspace key once, add env to
  each `PaneSpec`, append the pi args for matching panes.
- `src/web/api.rs` + `src/web/mod.rs`: the five routes, `X-MMUX` + `If-Match`
  enforcement, path-traversal-proof slug validation (slug is `[a-z0-9-]{1,80}`,
  never a path).
- `web/app.js` + `index.html` + `style.css`: toolbar entry, doc list, editor,
  search, conflict banner.
- `src/main.rs`: `Cli` gains a `mem` subcommand that runs headless and exits
  before any server/PAM setup.

Exit criteria: create/edit/search/delete docs from UI and CLI; two browsers
editing the same doc cannot silently clobber; `AGENT.md` regenerates within one
request; deleting `index` state loses nothing; a doc written by an agent mid-run
is visible to the UI without restart.

### Phase 2 — Archive (transcripts) — recall over history

- **pi transcript collector**: pi names its session file
  `<agent_dir>/sessions/--<cwd-slug>--/<ts>_<sessionId>.jsonl`, and mmux already
  knows the `sessionId` it passed. Read-only, incremental by byte offset, with
  reset on truncation/rewrite (compaction rewrites headers in place). Extract
  only: user prompts, assistant text, tool call names + primary argument, bash
  commands + exit codes, files touched, compaction boundaries. Never deserialize
  whole 4 MB files per query, and never copy raw JSONL.
- **terminal collector**: raise the vt100 parser's scrollback capacity
  (`Parser::new_with_callbacks(rows, cols, 0, …)` → `memory.scrollback_lines`,
  default 2000, tmux's default) and flush newly retired lines to
  `transcripts/…log` on a ~1 s timer from a dedicated thread. Alt-screen
  output is skipped (with a marker) so `vim`/`less` frames don't flood the log.
  Strip ANSI by taking plain text from the screen, not by regex.
- **index**: introduce `rusqlite` (`bundled`, FTS5) with `ScanIndex` retained as
  the fallback when `memory.index = "none"`. One writer task owns the write
  connection; readers use short-lived read-only connections; `busy_timeout`.
  Schema: `docs`, `doc_fts`, `sessions`, `messages`, `messages_fts`, `events`,
  `file_offsets`. Docs outrank transcripts (field weights + a kind/recency
  boost).
- **timeline UI/API**: sessions grouped by day per workspace — first prompt,
  duration, files touched, exit statuses — assembled deterministically from the
  collector output. This is the "human-facing archive" view.
- **retention**: `memory.retention_days`, `max_workspace_bytes`, per-pane log
  cap; background GC on a cooldown; `mmux mem prune` for manual runs. Closing a
  project never deletes its memory (memory is keyed by path, not project id).

Exit criteria: search across docs and history returns docs-first results; a
4 MB pi transcript is indexed in well under a second and re-index is
incremental; a pane producing 50 MB of output does not grow RSS unboundedly or
stall the PTY reader; killing the server loses at most ~1 s of transcript.

### Phase 3 — Retrieval and governance polish

- FTS5 `snippet()` highlighting; `--since`/`--kind`/`--tag` filters everywhere;
  per-workspace "include in search" opt-out to prevent the cross-project
  leakage failure mode.
- Staleness: `paths` in frontmatter are checked on `mmux mem stale` and shown
  as a warning in the UI ("2 of 3 referenced files no longer exist").
  `verified_at` is bumped by `mmux mem touch`.
- `supersedes` links; `mmux mem list` hides superseded docs behind `--all`.
- Generated pi skill (passed with `--skill`) so agents discover the workflow
  through skill metadata instead of prompt text — near-zero always-on cost.
- Optional `mmux mem digest` that assembles deterministic facts (prompts, files,
  commands, exits, diffs) into a markdown *skeleton* for the agent to refine and
  save deliberately. No background model calls; no auto-write.

## 6. Risks and decisions

| Risk | Mitigation |
|---|---|
| Memory bloat / stale docs | 40-line generated index, `stale`, `supersedes`, history cap, 64 KB/doc cap |
| Two writers (server + CLI) | Files are truth; index is derived and reconciled by `(mtime,size)`; `reindex` always available |
| Secrets in docs | Deliberate writes only; `0700`; pattern scan warns (does not block) on save; docs are never sent anywhere by mmux |
| Secrets in transcripts | `memory.terminal = "off" \| "stripped"`; never inject transcripts into prompts; retention + `prune` |
| Injected args break user commands | Injection only for `pi` basenames, spawn-time only, never persisted; `memory.inject = "none"` |
| SQLite build cost / portability | Phase 1 needs no new dependency; SQLite arrives only when transcripts make scanning impractical; `ScanIndex` stays as `memory.index = "none"` |
| Workspace identity churn (moves/renames) | Stored absolute path + `rekey`; no name-based keys |
| No scrollback restore into live panes | Intentional: TUI programs redraw; history is a separate, searchable surface |

Open question for review: whether `author = "agent:pi"` should be inferred (from
`MMUX_*` env) or always explicit, and whether the UI should allow creating docs
before any agent runs (I assume yes — it is the primary "user-demanded" path).

## 7. Verification

- Unit: frontmatter round-trip incl. unknown keys; slug collision; atomic write
  under simulated ENOSPC; history cap; `AGENT.md` truncation determinism;
  injection arg construction (pi vs. non-pi, never persisted).
- Integration (`tests/memory.rs`): CLI add/search/edit/rm against a temp state
  dir; concurrent CLI write + server read; `If-Match` 409; slug traversal
  attempts (`../`, absolute, unicode) rejected.
- Web (`tests/web.rs`): auth required, `X-MMUX` required, JSON-only bodies,
  doc content not served from the assets handler.
- Perf: 10k docs → search < 50 ms; 100 MB pane stream → bounded RSS, reader
  thread never blocked > 5 ms; incremental reindex of a growing pi transcript
  touches only appended bytes.
- Manual: `cargo fmt && cargo clippy --all-targets && cargo test`; run two
  browsers, edit the same doc, confirm the conflict path; `kill -9` the server
  mid-write and confirm the vault still loads.
