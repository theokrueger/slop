use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use tokio::sync::broadcast;
use vt100::{Callbacks, Parser, Screen};

use crate::config::Slot;
use crate::session::{PaneId, ProjectId, SessionEvent};
use crate::theme::{self, Appearance, Palette};

pub const MIN_ROWS: u16 = 5;
pub const MIN_COLS: u16 = 10;
pub const MAX_ROWS: u16 = 500;
pub const MAX_COLS: u16 = 1000;

const RESTART_DELAY: Duration = Duration::from_millis(250);

/// Everything needed to start a pane.
#[derive(Debug, Clone)]
pub struct PaneSpec {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub appearance: Appearance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneStatus {
    Running,
    Exited {
        code: Option<u32>,
        signal: Option<String>,
    },
}

impl PaneStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, PaneStatus::Running)
    }

    pub fn view(&self) -> PaneStatusView {
        match self {
            PaneStatus::Running => PaneStatusView::Running,
            PaneStatus::Exited { code, signal } => PaneStatusView::Exited {
                code: *code,
                signal: signal.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PaneStatusView {
    Running,
    Exited {
        code: Option<u32>,
        signal: Option<String>,
    },
}

/// A full terminal snapshot and the output sequence it represents.
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub data: String,
    pub seq: u64,
}

/// A chunk of raw PTY output, with a contiguous sequence range.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub start: u64,
    pub data: Arc<[u8]>,
}

impl OutputChunk {
    pub fn end(&self) -> u64 {
        self.start
            .saturating_add(self.data.len() as u64)
            .saturating_sub(1)
    }
}

/// One PTY-backed terminal. The pane outlives browser connections: its reader
/// and waiter threads are owned by the session, not by any WebSocket.
pub struct Pane {
    id: PaneId,
    project_id: ProjectId,
    slot: Slot,
    spec: PaneSpec,
    state: Mutex<PaneState>,
    process: Mutex<Option<Process>>,
    next_generation: AtomicU64,
    clients: Mutex<HashMap<u64, ClientSize>>,
    clipboard: broadcast::Sender<String>,
    output: broadcast::Sender<OutputChunk>,
    closed: AtomicBool,
    events: broadcast::Sender<SessionEvent>,
}

struct PaneState {
    parser: Parser<PaneCallbacks>,
    status: PaneStatus,
    output_seq: u64,
}

/// Answers the handful of terminal queries that interactive programs use to
/// discover cursor position and terminal capabilities. vt100 consumes these
/// sequences without replying, so we collect replies in a callback and send
/// them back to the PTY after parsing each chunk.
#[derive(Debug)]
struct PaneCallbacks {
    responses: String,
    clipboard: Vec<String>,
    palette: Palette,
}

impl Default for PaneCallbacks {
    fn default() -> Self {
        Self::new(Appearance::default())
    }
}

impl PaneCallbacks {
    fn new(appearance: Appearance) -> Self {
        Self {
            responses: String::new(),
            clipboard: Vec::new(),
            palette: theme::palette(appearance),
        }
    }

    fn osc_color(&mut self, code: &str, color: theme::Rgb) {
        use std::fmt::Write as _;
        let _ = write!(self.responses, "\x1b]{code};rgb:{}\x1b\\", color.xterm());
    }

    fn take_responses(&mut self) -> String {
        std::mem::take(&mut self.responses)
    }

    fn take_clipboard(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard)
    }

    fn cursor_report(&mut self, screen: &Screen, private: bool) {
        use std::fmt::Write as _;
        let (row, col) = screen.cursor_position();
        if private {
            let _ = write!(&mut self.responses, "\x1b[?{};{};1R", row + 1, col + 1);
        } else {
            let _ = write!(&mut self.responses, "\x1b[{};{}R", row + 1, col + 1);
        }
    }
}

impl Callbacks for PaneCallbacks {
    fn unhandled_osc(&mut self, _screen: &mut Screen, params: &[&[u8]]) {
        let Some(kind) = params.first().copied() else {
            return;
        };
        let second = params.get(1).copied();
        match (kind, second) {
            (b"10", Some(b"?")) => self.osc_color("10", self.palette.foreground),
            (b"11", Some(b"?")) => self.osc_color("11", self.palette.background),
            (b"12", Some(b"?")) => self.osc_color("12", self.palette.cursor),
            (b"4", Some(index)) if params.get(2).is_some_and(|query| *query == b"?") => {
                let Some(index) = std::str::from_utf8(index)
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                else {
                    return;
                };
                if let Some(color) = self.palette.ansi.get(index) {
                    use std::fmt::Write as _;
                    let _ = write!(self.responses, "\x1b]4;{index};rgb:{}\x1b\\", color.xterm());
                }
            }
            _ => {}
        }
    }

    fn copy_to_clipboard(&mut self, _screen: &mut Screen, _ty: &[u8], data: &[u8]) {
        if data.len() > 1_000_000 {
            return;
        }
        let decoded = BASE64.decode(data).or_else(|_| BASE64_NO_PAD.decode(data));
        let Ok(decoded) = decoded else {
            return;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return;
        };
        if !text.is_empty() {
            self.clipboard.push(text);
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        use std::fmt::Write as _;
        let first = params.first().and_then(|param| param.first()).copied();
        match (i1, c) {
            (None, 'n') => match first {
                Some(5) => self.responses.push_str("\x1b[0n"),
                Some(6) => self.cursor_report(screen, false),
                _ => {}
            },
            (Some(b'?'), 'n') if first == Some(6) => self.cursor_report(screen, true),
            (None, 'c') if first.is_none() || first == Some(0) => {
                self.responses.push_str("\x1b[?64;1;2;6;9;15;18;21;22c")
            }
            (Some(b'>'), 'c') if first.is_none() || first == Some(0) => {
                self.responses.push_str("\x1b[>41;370;0c");
            }
            (None, 't') if first == Some(18) || first == Some(19) => {
                let (rows, cols) = screen.size();
                let code = if first == Some(18) { 8 } else { 9 };
                let _ = write!(
                    &mut self.responses,
                    "\x1b[{code};{};{}t",
                    rows.get(),
                    cols.get()
                );
            }
            _ => {}
        }
    }
}

struct Process {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    pgid: Option<i32>,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct ClientSize {
    cols: u16,
    rows: u16,
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Normally the waiter thread has already taken the process handle,
        // but if thread creation ever failed this prevents an orphan.
        self.closed.store(true, Ordering::Release);
        self.signal(Signal::SIGKILL);
    }
}

impl Pane {
    /// Spawns the pane and supervises its command. Exits and startup failures
    /// are reported in the pane before the same command is tried again.
    pub fn spawn(
        id: PaneId,
        project_id: ProjectId,
        slot: Slot,
        spec: PaneSpec,
        size: (u16, u16),
        events: broadcast::Sender<SessionEvent>,
    ) -> Arc<Self> {
        let (rows, cols) = sanitize_size(size.0, size.1);
        let parser = Parser::new_with_callbacks(rows, cols, 0, PaneCallbacks::new(spec.appearance));
        let (clipboard, _) = broadcast::channel(64);
        let (output, _) = broadcast::channel(512);
        let started = Self::start_process(&spec, rows.get(), cols.get());
        let pane = Arc::new(Self {
            id,
            project_id,
            slot,
            spec,
            state: Mutex::new(PaneState {
                parser,
                status: PaneStatus::Running,
                output_seq: 0,
            }),
            process: Mutex::new(None),
            next_generation: AtomicU64::new(0),
            clients: Mutex::new(HashMap::new()),
            clipboard,
            output,
            closed: AtomicBool::new(false),
            events,
        });

        match started {
            Ok((mut process, reader)) => {
                let generation = pane.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
                process.generation = generation;
                *pane.process.lock() = Some(process);
                pane.spawn_reader(reader, generation);
            }
            Err(err) => {
                tracing::warn!(
                    pane = id.get(),
                    command = ?pane.full_command(),
                    error = %err,
                    "failed to start pane"
                );
                pane.fail(&err);
            }
        }
        pane.spawn_waiter();
        pane
    }

    fn restart_process(self: &Arc<Self>) {
        if self.is_closed() {
            return;
        }
        let (rows, cols) = self.size();
        match Self::start_process(&self.spec, rows, cols) {
            Ok((mut process, reader)) => {
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
                process.generation = generation;
                let mut current = self.process.lock();
                // Use the latest size: a client may have resized the pane while
                // the new process was starting and there was no PTY to resize.
                let (rows, cols) = self.size();
                if let Err(err) = process.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    tracing::warn!(
                        pane = self.id.get(),
                        error = %err,
                        "could not resize restarted PTY"
                    );
                }
                // Always install the handle so the waiter can reap the child,
                // including when termination raced with process startup.
                *current = Some(process);
                drop(current);
                self.state.lock().status = PaneStatus::Running;
                if self.is_closed() {
                    self.signal(Signal::SIGKILL);
                    return;
                }
                tracing::info!(
                    pane = self.id.get(),
                    command = ?self.full_command(),
                    "restarted pane command"
                );
                self.spawn_reader(reader, generation);
                let _ = self.events.send(SessionEvent::PaneRestarted {
                    project_id: self.project_id,
                    pane_id: self.id,
                });
            }
            Err(err) => {
                tracing::warn!(
                    pane = self.id.get(),
                    command = ?self.full_command(),
                    error = %err,
                    "failed to restart pane"
                );
                self.fail(&err);
            }
        }
    }

    fn start_process(
        spec: &PaneSpec,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<(Process, Box<dyn Read + Send>)> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("could not open a PTY")?;

        let (program, args) = spec
            .command
            .split_first()
            .map(|(program, args)| (program.as_str(), args))
            .unwrap_or(("/bin/sh", &[][..]));

        let mut command = CommandBuilder::new(program);
        for arg in args {
            command.arg(arg);
        }
        command.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "mmux");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));

        // Complete all fallible PTY setup before spawning. After this point,
        // the child is always returned in a Process and owned by the waiter.
        let reader = pair
            .master
            .try_clone_reader()
            .context("could not read from PTY")?;
        let writer = pair
            .master
            .take_writer()
            .context("could not write to PTY")?;
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("could not start `{program}`"))?;
        drop(pair.slave);
        let pgid = pair
            .master
            .process_group_leader()
            .or_else(|| child.process_id().and_then(|pid| i32::try_from(pid).ok()));

        Ok((
            Process {
                master: pair.master,
                writer,
                child,
                pgid,
                generation: 0,
            },
            reader,
        ))
    }

    fn spawn_reader(self: &Arc<Self>, mut reader: Box<dyn Read + Send>, generation: u64) {
        let pane = Arc::clone(self);
        let name = format!("mmux-reader-{}-{generation}", self.id.get());
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let (responses, clipboard) = {
                            let mut state = pane.state.lock();
                            state.parser.process(&buffer[..count]);
                            state.output_seq += count as u64;
                            let start = state.output_seq - count as u64 + 1;
                            let chunk = OutputChunk {
                                start,
                                data: Arc::<[u8]>::from(&buffer[..count]),
                            };
                            // Keep chunks in sequence-number order when an old
                            // reader drains while a new generation is running.
                            let _ = pane.output.send(chunk);
                            let callbacks = state.parser.callbacks_mut();
                            (callbacks.take_responses(), callbacks.take_clipboard())
                        };
                        for text in clipboard {
                            let _ = pane.clipboard.send(text);
                        }
                        if !responses.is_empty()
                            && let Err(err) =
                                pane.write_input_for_generation(generation, responses.as_bytes())
                        {
                            tracing::debug!(
                                pane = pane.id.get(),
                                error = %err,
                                "could not answer terminal query"
                            );
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        if let Err(err) = spawned {
            tracing::error!(pane = self.id.get(), error = %err, "could not start pane reader");
        }
    }

    fn spawn_waiter(self: &Arc<Self>) {
        let pane = Arc::clone(self);
        let name = format!("mmux-waiter-{}", self.id.get());
        let spawned = std::thread::Builder::new().name(name).spawn(move || loop {
            let status = {
                let mut process = pane.process.lock();
                match process.as_mut() {
                    Some(process) => process.child.try_wait(),
                    None => {
                        drop(process);
                        if pane.is_closed() {
                            return;
                        }
                        std::thread::sleep(RESTART_DELAY);
                        if !pane.is_closed() {
                            pane.restart_process();
                        }
                        continue;
                    }
                }
            };
            match status {
                Ok(Some(status)) => {
                    let signal = status.signal().map(str::to_owned);
                    let code = signal.is_none().then(|| status.exit_code());
                    pane.finish(code, signal);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(err) => {
                    // A wait error is not proof that the child exited. Keep the
                    // handle and retry rather than starting a duplicate process.
                    tracing::warn!(pane = pane.id.get(), error = %err, "could not wait for pane");
                    std::thread::sleep(RESTART_DELAY);
                }
            }
        });
        if let Err(err) = spawned {
            tracing::error!(pane = self.id.get(), error = %err, "could not start pane waiter");
        }
    }

    fn fail(&self, err: &anyhow::Error) {
        let message = format!(
            "\r\nmmux: could not start `{}`\r\n{}: {err}\r\n",
            self.full_command(),
            self.spec.cwd.display()
        );
        {
            let mut state = self.state.lock();
            state.parser.process(message.as_bytes());
            state.output_seq += message.len() as u64;
            let start = state.output_seq - message.len() as u64 + 1;
            let _ = self.output.send(OutputChunk {
                start,
                data: Arc::<[u8]>::from(message.as_bytes()),
            });
            state.status = PaneStatus::Exited {
                code: None,
                signal: None,
            };
        }
        let _ = self.events.send(SessionEvent::PaneExited {
            project_id: self.project_id,
            pane_id: self.id,
        });
    }

    fn finish(&self, code: Option<u32>, signal: Option<String>) {
        {
            let mut state = self.state.lock();
            if !state.status.is_running() {
                return;
            }
            state.status = PaneStatus::Exited { code, signal };
        }
        // Drop the PTY handles now that the child is gone. The cloned reader
        // drains any remaining output before it observes EOF.
        let _ = self.process.lock().take();
        let _ = self.events.send(SessionEvent::PaneExited {
            project_id: self.project_id,
            pane_id: self.id,
        });
    }

    pub fn id(&self) -> PaneId {
        self.id
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.spec.cwd
    }

    pub fn command(&self) -> &[String] {
        &self.spec.command
    }

    pub fn status(&self) -> PaneStatus {
        self.state.lock().status.clone()
    }

    pub fn is_running(&self) -> bool {
        self.status().is_running()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Visible screen size as `(rows, cols)`.
    pub fn size(&self) -> (u16, u16) {
        let state = self.state.lock();
        let (rows, cols) = state.parser.screen().size();
        (rows.get(), cols.get())
    }

    /// Plain-text contents, useful for diagnostics and tests.
    pub fn contents(&self) -> String {
        self.state.lock().parser.screen().contents()
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock();
        let screen = state.parser.screen();
        let (rows, cols) = screen.size();
        let mut data = String::new();
        // Snapshots are applied after a client-side reset, so reproduce the
        // alternate-screen selection along with the visible contents.
        if screen.alternate_screen() {
            data.push_str("\x1b[?1049h");
        } else {
            data.push_str("\x1b[?1049l");
        }
        data.push_str(&screen.state_formatted());
        Snapshot {
            rows: rows.get(),
            cols: cols.get(),
            data,
            seq: state.output_seq,
        }
    }

    /// Clipboard writes requested by the pane's program via OSC 52.
    pub fn subscribe_clipboard(&self) -> broadcast::Receiver<String> {
        self.clipboard.subscribe()
    }

    /// Raw PTY output for live browser clients.
    pub fn subscribe_output(&self) -> broadcast::Receiver<OutputChunk> {
        self.output.subscribe()
    }

    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_input_inner(None, bytes)
    }

    fn write_input_for_generation(&self, generation: u64, bytes: &[u8]) -> io::Result<()> {
        self.write_input_inner(Some(generation), bytes)
    }

    fn write_input_inner(&self, generation: Option<u64>, bytes: &[u8]) -> io::Result<()> {
        let mut process = self.process.lock();
        let Some(process) = process.as_mut() else {
            return Ok(());
        };
        // A response produced by an old reader must never be injected into a
        // replacement process merely because descendants kept its PTY open.
        if generation.is_some_and(|expected| process.generation != expected) {
            return Ok(());
        }
        process.writer.write_all(bytes)?;
        process.writer.flush()
    }

    /// Registers a browser client's desired viewport and returns the new
    /// effective `(rows, cols)`. Like tmux, the smallest attached client wins.
    pub fn attach_client(&self, client: u64, cols: u16, rows: u16) -> (u16, u16) {
        let (cols, rows) = (clamp_cols(cols), clamp_rows(rows));
        let mut clients = self.clients.lock();
        clients.insert(client, ClientSize { cols, rows });
        let (cols, rows) = effective_size(&clients);
        drop(clients);
        self.apply_size(cols, rows);
        self.size()
    }

    pub fn update_client(&self, client: u64, cols: u16, rows: u16) -> (u16, u16) {
        self.attach_client(client, cols, rows)
    }

    /// Removes a client. With no clients left, the pane keeps its last size.
    pub fn detach_client(&self, client: u64) {
        let mut clients = self.clients.lock();
        clients.remove(&client);
        if clients.is_empty() {
            return;
        }
        let (cols, rows) = effective_size(&clients);
        drop(clients);
        self.apply_size(cols, rows);
    }

    fn apply_size(&self, cols: u16, rows: u16) {
        let (rows, cols) = sanitize_size(rows, cols);
        {
            let mut state = self.state.lock();
            let (current_rows, current_cols) = state.parser.screen().size();
            if current_rows == rows && current_cols == cols {
                return;
            }
            state.parser.set_size(rows, cols);
        }
        if let Some(process) = self.process.lock().as_ref() {
            let result = process.master.resize(PtySize {
                rows: rows.get(),
                cols: cols.get(),
                pixel_width: 0,
                pixel_height: 0,
            });
            if let Err(err) = result {
                tracing::warn!(pane = self.id.get(), error = %err, "could not resize PTY");
            }
        }
    }

    /// Politely stops the pane's process group, escalating to SIGKILL after a
    /// short grace period.
    pub fn terminate(self: &Arc<Self>) {
        self.closed.store(true, Ordering::Release);
        self.signal(Signal::SIGTERM);
        let pane = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("mmux-reaper-{}", self.id.get()))
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                if pane.is_running() {
                    pane.signal(Signal::SIGKILL);
                }
            });
        if let Err(err) = spawned {
            tracing::error!(pane = self.id.get(), error = %err, "could not start pane reaper");
        }
    }

    /// Immediately kills the pane's process group.
    pub fn kill(&self) {
        self.closed.store(true, Ordering::Release);
        self.signal(Signal::SIGKILL);
    }

    fn signal(&self, signal: Signal) {
        let process = self.process.lock();
        if let Some(process) = process.as_ref() {
            Self::signal_process(self.id, process, signal);
        }
    }

    fn signal_process(id: PaneId, process: &Process, signal: Signal) {
        if let Some(pgid) = process.pgid.filter(|pgid| *pgid > 1) {
            if let Err(err) = killpg(Pid::from_raw(pgid), signal) {
                tracing::debug!(pane = id.get(), %pgid, %signal, error = %err, "killpg failed");
            }
        } else if let Some(pid) = process
            .child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
        {
            let _ = kill(Pid::from_raw(pid), signal);
        }
    }

    fn full_command(&self) -> String {
        if self.spec.command.is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        } else {
            self.spec.command.join(" ")
        }
    }
}

fn effective_size(clients: &HashMap<u64, ClientSize>) -> (u16, u16) {
    let cols = clients.values().map(|c| c.cols).min().unwrap_or(MAX_COLS);
    let rows = clients.values().map(|c| c.rows).min().unwrap_or(MAX_ROWS);
    (cols, rows)
}

fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(MIN_COLS, MAX_COLS)
}

fn clamp_rows(rows: u16) -> u16 {
    rows.clamp(MIN_ROWS, MAX_ROWS)
}

fn sanitize_size(rows: u16, cols: u16) -> (std::num::NonZeroU16, std::num::NonZeroU16) {
    let rows = std::num::NonZeroU16::new(clamp_rows(rows)).unwrap_or(std::num::NonZeroU16::MIN);
    let cols = std::num::NonZeroU16::new(clamp_cols(cols)).unwrap_or(std::num::NonZeroU16::MIN);
    (rows, cols)
}

/// Resolves an empty command to the user's shell.
pub fn resolve_command(command: &[String]) -> anyhow::Result<Vec<String>> {
    if !command.is_empty() {
        return Ok(command.to_vec());
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    if shell.contains('\0') {
        return Err(anyhow!("$SHELL contains a NUL byte"));
    }
    Ok(vec![shell])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spec(command: Vec<&str>, cwd: &std::path::Path) -> PaneSpec {
        PaneSpec {
            command: command.into_iter().map(str::to_owned).collect(),
            env: BTreeMap::new(),
            cwd: cwd.to_path_buf(),
            appearance: Appearance::Dark,
        }
    }

    fn test_pane(command: Vec<&str>, cwd: &std::path::Path) -> Arc<Pane> {
        let (events, _) = broadcast::channel(16);
        Pane::spawn(
            PaneId::new(1),
            ProjectId::new(1),
            Slot::TopLeft,
            spec(command, cwd),
            (24, 80),
            events,
        )
    }

    fn wait_for(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        predicate()
    }

    fn cwd() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn runs_in_the_requested_directory() {
        let dir = cwd();
        let pane = test_pane(vec!["sh", "-c", "pwd; sleep 5"], dir.path());
        assert!(
            wait_for(
                || pane
                    .contents()
                    .contains(&dir.path().to_string_lossy().to_string()),
                Duration::from_secs(5)
            ),
            "contents: {:?}",
            pane.contents()
        );
        pane.kill();
    }

    #[test]
    fn accepts_input_and_uses_shared_screen_state() {
        let dir = cwd();
        let pane = test_pane(vec!["sh"], dir.path());
        assert!(wait_for(
            || !pane.contents().trim().is_empty(),
            Duration::from_secs(5)
        ));
        pane.write_input(b"printf 'MARKER-123\\n'\n").unwrap();
        assert!(
            wait_for(
                || pane.contents().contains("MARKER-123"),
                Duration::from_secs(5)
            ),
            "contents: {:?}",
            pane.contents()
        );
        pane.kill();
    }

    #[test]
    fn snapshots_and_raw_chunks_cover_output() {
        let dir = cwd();
        let pane = test_pane(vec!["sh"], dir.path());
        assert!(wait_for(
            || !pane.contents().trim().is_empty(),
            Duration::from_secs(5)
        ));
        let mut output = pane.subscribe_output();
        pane.write_input(b"printf 'RAW_HELLO\\n'\n").unwrap();
        let mut text = String::new();
        assert!(
            wait_for(
                || {
                    while let Ok(chunk) = output.try_recv() {
                        text.push_str(&String::from_utf8_lossy(&chunk.data));
                    }
                    text.contains("RAW_HELLO")
                },
                Duration::from_secs(5)
            ),
            "raw output: {text:?}"
        );
        let snapshot = pane.snapshot();
        assert!(snapshot.data.contains("RAW_HELLO"));
        assert!(snapshot.seq > 0);
        pane.kill();
    }

    #[test]
    fn smallest_client_controls_the_size_and_last_size_is_kept() {
        let dir = cwd();
        let pane = test_pane(vec!["sleep", "10"], dir.path());
        assert_eq!(pane.attach_client(1, 100, 30), (30, 100));
        assert_eq!(pane.attach_client(2, 80, 24), (24, 80));
        assert_eq!(pane.update_client(1, 120, 40), (24, 80));
        pane.detach_client(1);
        assert_eq!(pane.size(), (24, 80));
        pane.detach_client(2);
        assert_eq!(pane.size(), (24, 80));
        pane.kill();
    }

    #[test]
    fn reports_exit_status() {
        let dir = cwd();
        let pane = test_pane(vec!["sh", "-c", "exit 7"], dir.path());
        assert!(
            wait_for(
                || matches!(pane.status(), PaneStatus::Exited { code: Some(7), .. }),
                Duration::from_secs(5)
            ),
            "status: {:?}",
            pane.status()
        );
        pane.kill();
    }

    #[test]
    fn restarts_the_same_command_after_it_exits() {
        let dir = cwd();
        let marker = dir.path().join("started");
        let pane = test_pane(
            vec![
                "sh",
                "-c",
                "if [ -e \"$1\" ]; then printf 'RESTARTED\\n'; sleep 60; fi; touch \"$1\"; exit 0",
                "mmux-test",
                marker.to_str().unwrap(),
            ],
            dir.path(),
        );
        assert!(
            wait_for(
                || pane.contents().contains("RESTARTED"),
                Duration::from_secs(5)
            ),
            "contents: {:?}",
            pane.contents()
        );
        assert!(pane.is_running());
        assert_eq!(
            pane.command(),
            &[
                "sh",
                "-c",
                "if [ -e \"$1\" ]; then printf 'RESTARTED\\n'; sleep 60; fi; touch \"$1\"; exit 0",
                "mmux-test",
                marker.to_str().unwrap(),
            ]
        );
        pane.kill();
    }

    #[test]
    fn missing_program_is_reported_in_the_pane() {
        let dir = cwd();
        let pane = test_pane(vec!["definitely-not-a-real-program-mmux"], dir.path());
        assert!(wait_for(
            || pane.contents().contains("could not start"),
            Duration::from_secs(5)
        ));
        assert!(matches!(
            pane.status(),
            PaneStatus::Exited { code: None, .. }
        ));
        pane.kill();
    }

    #[test]
    fn terminate_stops_the_process_group() {
        let dir = cwd();
        let pane = test_pane(vec!["sleep", "60"], dir.path());
        assert!(pane.is_running());
        pane.terminate();
        assert!(
            wait_for(|| !pane.is_running(), Duration::from_secs(4)),
            "pane did not stop"
        );
    }

    #[test]
    fn empty_command_uses_the_shell() {
        let resolved = resolve_command(&[]).unwrap();
        assert!(!resolved.is_empty());
    }

    fn query(bytes: &[u8]) -> String {
        let rows = std::num::NonZeroU16::new(24).unwrap();
        let cols = std::num::NonZeroU16::new(80).unwrap();
        let mut parser = Parser::new_with_callbacks(rows, cols, 0, PaneCallbacks::default());
        parser.process(bytes);
        parser.callbacks_mut().take_responses()
    }

    #[test]
    fn answers_cursor_position_reports() {
        assert_eq!(query(b"abc\x1b[6n"), "\x1b[1;4R");
        assert_eq!(query(b"abc\x1b[?6n"), "\x1b[?1;4;1R");
    }

    #[test]
    fn answers_cursor_queries_over_the_pty() {
        let dir = cwd();
        let pane = test_pane(
            vec![
                "sh",
                "-c",
                "stty raw -echo; printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -tx1; sleep 5",
            ],
            dir.path(),
        );
        assert!(
            wait_for(
                || pane.contents().contains("1b 5b 31 3b 31 52"),
                Duration::from_secs(5)
            ),
            "contents: {:?}",
            pane.contents()
        );
        pane.kill();
    }

    #[test]
    fn decodes_osc52_clipboard_writes() {
        let mut parser = Parser::new_with_callbacks(
            std::num::NonZeroU16::new(24).unwrap(),
            std::num::NonZeroU16::new(80).unwrap(),
            0,
            PaneCallbacks::default(),
        );
        parser.process(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(parser.callbacks_mut().take_clipboard(), vec!["hello"]);
        parser.process(b"\x1b]52;c;aGVsbG8\x07");
        assert_eq!(parser.callbacks_mut().take_clipboard(), vec!["hello"]);
        parser.process(b"\x1b]52;c;not base64!\x07");
        assert!(parser.callbacks_mut().take_clipboard().is_empty());
    }

    #[test]
    fn forwards_osc52_writes_from_the_pty() {
        let dir = cwd();
        let pane = test_pane(vec!["sh"], dir.path());
        assert!(
            wait_for(
                || !pane.contents().trim().is_empty(),
                Duration::from_secs(5)
            ),
            "shell did not start"
        );
        // Subscribe before sending the command so the broadcast cannot race
        // the parser callback.
        let mut clipboard = pane.subscribe_clipboard();
        pane.write_input(b"printf '\\033]52;c;aGVsbG8=\\a'\n")
            .unwrap();
        let mut received = None;
        assert!(
            wait_for(
                || match clipboard.try_recv() {
                    Ok(text) => {
                        received = Some(text);
                        true
                    }
                    Err(broadcast::error::TryRecvError::Empty) => false,
                    Err(_) => true,
                },
                Duration::from_secs(5)
            ),
            "no OSC 52 clipboard write observed; status={:?} contents={:?}",
            pane.status(),
            pane.contents()
        );
        assert_eq!(received.as_deref(), Some("hello"));
        pane.kill();
    }

    #[test]
    fn answers_osc_color_queries() {
        let mut parser = Parser::new_with_callbacks(
            std::num::NonZeroU16::new(24).unwrap(),
            std::num::NonZeroU16::new(80).unwrap(),
            0,
            PaneCallbacks::new(Appearance::Dark),
        );
        parser.process(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
        let responses = parser.callbacks_mut().take_responses();
        assert!(
            responses.contains("\x1b]10;rgb:9393/a1a1/a1a1\x1b\\"),
            "{responses:?}"
        );
        assert!(
            responses.contains("\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
            "{responses:?}"
        );
        assert!(
            responses.contains("\x1b]4;1;rgb:dcdc/3232/2f2f\x1b\\"),
            "{responses:?}"
        );
    }

    #[test]
    fn answers_status_and_capability_queries() {
        assert_eq!(query(b"\x1b[5n"), "\x1b[0n");
        assert_eq!(query(b"\x1b[c"), "\x1b[?64;1;2;6;9;15;18;21;22c");
        assert_eq!(query(b"\x1b[>c"), "\x1b[>41;370;0c");
        assert_eq!(query(b"\x1b[18t"), "\x1b[8;24;80t");
        assert_eq!(query(b"\x1b[19t"), "\x1b[9;24;80t");
        assert_eq!(query(b"\x1b[99n"), "");
    }
}
