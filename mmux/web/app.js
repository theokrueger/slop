(() => {
  "use strict";

  const TERM_FONT = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';
  const CLIENT_RESIZE = 0;
  const CLIENT_INPUT = 1;
  const SERVER_FULL = 0;
  const SERVER_CLIPBOARD = 2;
  const SERVER_DATA = 3;

  const decoder = new TextDecoder();
  const latin1Decoder = new TextDecoder("latin1");
  // Only queries that mmux answers server-side need their xterm auto-response
  // suppressed. This avoids duplicate replies reaching the program.
  const TERMINAL_QUERY = /\x1b\[(?:5n|6n|\?6n|(?:0)?c|>0?c)/g;

  // Slot coordinates within the pane grid: [row, column].
  const PANE_GRID = {
    "top-left": [0, 0],
    "top-right": [0, 1],
    "bottom-left": [1, 0],
    "bottom-right": [1, 1],
  };
  const ARROW_DIRECTIONS = {
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
  };
  const MEMORY_KINDS = ["note", "decision", "plan", "runbook", "retro", "reference"];

  const els = {
    loading: document.getElementById("loading"),
    app: document.getElementById("app"),
    projects: document.getElementById("projects"),
    projectManagement: document.getElementById("project-management"),
    newProject: document.getElementById("new-project"),
    archiveToggle: document.getElementById("archive-toggle"),
    memoryDialog: document.getElementById("memory-dialog"),
    memoryDialogTitle: document.getElementById("memory-dialog-title"),
    memoryStatus: document.getElementById("memory-status"),
    memoryStatusMessage: document.getElementById("memory-status-message"),
    memoryStatusAdvice: document.getElementById("memory-status-advice"),
    memoryStatusRecheck: document.getElementById("memory-status-recheck"),
    memorySearch: document.getElementById("memory-search"),
    memoryNew: document.getElementById("memory-new"),
    memoryArchiveToggle: document.getElementById("memory-archive-toggle"),
    memoryArchiveDialog: document.getElementById("memory-archive-dialog"),
    memoryArchiveError: document.getElementById("memory-archive-error"),
    memoryArchiveRetry: document.getElementById("memory-archive-retry"),
    memoryArchiveEmpty: document.getElementById("memory-archive-empty"),
    memoryArchiveList: document.getElementById("memory-archive-list"),
    memoryArchiveClose: document.getElementById("memory-archive-close"),
    memoryList: document.getElementById("memory-list"),
    memoryListError: document.getElementById("memory-list-error"),
    memoryEditor: document.getElementById("memory-editor"),
    memoryTitle: document.getElementById("memory-title"),
    memoryKind: document.getElementById("memory-kind"),
    memoryTags: document.getElementById("memory-tags"),
    memoryBody: document.getElementById("memory-body"),
    memoryEditorError: document.getElementById("memory-editor-error"),
    memorySave: document.getElementById("memory-save"),
    memoryDelete: document.getElementById("memory-delete"),
    memoryReload: document.getElementById("memory-reload"),
    memoryCancel: document.getElementById("memory-cancel"),
    memoryClose: document.getElementById("memory-close"),
    archiveDialog: document.getElementById("archive-dialog"),
    archiveError: document.getElementById("archive-error"),
    archiveRetry: document.getElementById("archive-retry"),
    archiveEmpty: document.getElementById("archive-empty"),
    archiveList: document.getElementById("archive-list"),
    archiveClose: document.getElementById("archive-close"),
    user: document.getElementById("user"),
    logout: document.getElementById("logout"),
    main: document.getElementById("main"),
    panes: document.getElementById("panes"),
    emptyState: document.getElementById("empty-state"),
    emptyNewProject: document.getElementById("empty-new-project"),
    newProjectDialog: document.getElementById("new-project-dialog"),
    newProjectForm: document.getElementById("new-project-form"),
    projectName: document.getElementById("new-project-name"),
    dirPath: document.getElementById("dir-path"),
    dirCurrent: document.getElementById("dir-current"),
    dirList: document.getElementById("dir-list"),
    newProjectDialogError: document.getElementById("dialog-error"),
    newProjectDialogCancel: document.getElementById("dialog-cancel"),
    newProjectDialogCreate: document.getElementById("dialog-create"),
    toast: document.getElementById("toast"),
  };

  const state = {
    projects: [],
    appearance: "dark",
    activeProjectId: null,
    user: null,
    root: null,
    handles: new Map(),
    renderedProjectKey: null,
    editingProjectId: null,
    draggedProjectId: null,
    projectDropTarget: null,
    refreshTimer: null,
    refreshing: false,
    pendingRefresh: false,
    events: null,
    pickerRequest: 0,
    pickerLoading: false,
    memory: {
      status: null,
      docs: [],
      hits: [],
      query: "",
      searchToken: 0,
      searchTimer: 0,
      editingDoc: null,
      editingWorkspace: null,
      loadedFor: null,
      archived: [],
    },
    archive: {
      data: null,
      error: null,
      count: 0,
    },
  };

  const encoder = new TextEncoder();

  async function api(path, { method = "GET", body, mutation = false, headers: extraHeaders } = {}) {
    const headers = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (mutation) headers["X-MMUX"] = "1";
    if (extraHeaders) Object.assign(headers, extraHeaders);
    const response = await fetch(path, {
      method,
      headers,
      credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    let data = null;
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
    }
    if (response.status === 401 && path !== "login") {
      window.location.replace("login");
      throw new Error("unauthorized");
    }
    if (!response.ok) {
      let message = data && data.error ? data.error : `request failed (${response.status})`;
      if (response.status === 429) message = "Too many attempts. Try again in a few seconds.";
      const error = new Error(message);
      error.status = response.status;
      throw error;
    }
    return data;
  }

  const SOLARIZED = {
    base03: "#002b36",
    base02: "#073642",
    base01: "#586e75",
    base00: "#657b83",
    base0: "#839496",
    base1: "#93a1a1",
    base2: "#eee8d5",
    base3: "#fdf6e3",
    yellow: "#b58900",
    orange: "#cb4b16",
    red: "#dc322f",
    magenta: "#d33682",
    violet: "#6c71c4",
    blue: "#268bd2",
    cyan: "#2aa198",
    green: "#859900",
  };

  const SOLARIZED_ANSI = {
    black: SOLARIZED.base02,
    red: SOLARIZED.red,
    green: SOLARIZED.green,
    yellow: SOLARIZED.yellow,
    blue: SOLARIZED.blue,
    magenta: SOLARIZED.magenta,
    cyan: SOLARIZED.cyan,
    white: SOLARIZED.base2,
    // Canonical Solarized maps the bright slots to base tones, which makes
    // standard prompts such as `01;32`/`01;34` look gray. Keep actual accent
    // colors here so ANSI colors remain visible.
    brightBlack: SOLARIZED.base01,
    brightRed: SOLARIZED.orange,
    brightGreen: SOLARIZED.green,
    brightYellow: SOLARIZED.yellow,
    brightBlue: SOLARIZED.blue,
    brightMagenta: SOLARIZED.violet,
    brightCyan: SOLARIZED.cyan,
    brightWhite: SOLARIZED.base1,
  };

  function termTheme() {
    if (state.appearance === "dark") {
      return {
        ...SOLARIZED_ANSI,
        background: SOLARIZED.base03,
        foreground: SOLARIZED.base1,
        cursor: SOLARIZED.base1,
        cursorAccent: SOLARIZED.base03,
        selectionBackground: SOLARIZED.base02,
      };
    }
    return {
      ...SOLARIZED_ANSI,
      brightWhite: SOLARIZED.base1,
      background: SOLARIZED.base3,
      foreground: SOLARIZED.base00,
      cursor: SOLARIZED.base00,
      cursorAccent: SOLARIZED.base3,
      selectionBackground: SOLARIZED.base2,
    };
  }

  let toastTimer = 0;

  function showToast(message, action) {
    window.clearTimeout(toastTimer);
    els.toast.replaceChildren();
    const label = document.createElement("span");
    label.textContent = message;
    els.toast.append(label);
    if (action) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "toast-action";
      button.textContent = action.label;
      button.addEventListener("click", (event) => {
        event.preventDefault();
        action.run();
      });
      els.toast.append(button);
    }
    els.toast.hidden = false;
    toastTimer = window.setTimeout(() => {
      els.toast.hidden = true;
    }, action ? 10000 : 1800);
  }

  async function writeClipboard(text) {
    if (!text) return false;
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch {
      // Fall back to the legacy copy path below.
    }

    const active = document.activeElement;
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.top = "-1000px";
    area.style.left = "-1000px";
    document.body.append(area);
    area.select();
    area.setSelectionRange(0, area.value.length);
    let copied = false;
    try {
      copied = document.execCommand("copy");
    } catch {
      copied = false;
    }
    area.remove();
    if (active && typeof active.focus === "function") {
      active.focus({ preventScroll: true });
    }
    return copied;
  }

  async function copyText(text) {
    if (!text) return false;
    if (await writeClipboard(text)) {
      showToast("Copied to clipboard");
      return true;
    }
    showToast("Clipboard write was blocked.", {
      label: "Copy",
      run: () => {
        void copyText(text);
      },
    });
    return false;
  }

  async function refresh() {
    if (state.refreshing) {
      state.pendingRefresh = true;
      return;
    }
    state.refreshing = true;
    try {
      const data = await api("api/state");
      const appearance = data.appearance === "light" ? "light" : "dark";
      if (state.appearance !== appearance) {
        state.appearance = appearance;
        for (const handle of state.handles.values()) {
          handle.term.options.theme = termTheme();
        }
      }
      applyAppearance();
      state.projects = data.projects;
      state.user = data.user;
      state.root = data.workspace_root;
      if (!state.projects.some((project) => project.id === state.activeProjectId)) {
        state.activeProjectId = state.projects.length ? state.projects[0].id : null;
      }
      render();
      els.loading.hidden = true;
      els.app.hidden = false;
      connectEvents();
    } catch (error) {
      if (error.message !== "unauthorized") {
        els.loading.hidden = false;
        els.loading.textContent = `could not connect to mmux: ${error.message}`;
        window.setTimeout(() => refresh(), 3000);
      }
    } finally {
      state.refreshing = false;
      if (state.pendingRefresh) {
        state.pendingRefresh = false;
        scheduleRefresh(20);
      }
    }
  }

  function applyAppearance() {
    const light = state.appearance === "light";
    document.documentElement.classList.toggle("theme-light", light);
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", light ? "#fdf6e3" : "#002b36");
  }

  function scheduleRefresh(delay = 60) {
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = window.setTimeout(refresh, delay);
  }

  function connectEvents() {
    if (state.events) return;
    state.events = new EventSource("api/events");
    state.events.onmessage = (event) => {
      let payload = null;
      try {
        payload = JSON.parse(event.data);
      } catch {
        // Not our shape; fall through to a full refresh.
      }
      if (payload && payload.type === "memory_changed") {
        if (els.memoryArchiveDialog.open) void loadMemoryArchive();
        if (state.memory.query) {
          void runMemorySearch();
        } else {
          void loadMemoryDocs();
        }
        return;
      }
      scheduleRefresh();
    };
    state.events.onerror = () => {
      // EventSource reconnects automatically; re-sync on the next event.
    };
    // A slow safety net for missed events or a restarted server.
    window.setInterval(() => {
      if (!document.hidden) scheduleRefresh(0);
    }, 30000);
  }

  function render() {
    els.user.textContent = state.user || "";
    if (!state.editingProjectId) renderProjects();
    renderActiveProject();
    els.emptyState.hidden = state.projects.length !== 0;
    els.panes.hidden = state.projects.length === 0;
  }

  function renderProjects() {
    els.projects.replaceChildren();
    for (const project of state.projects) {
      const item = document.createElement("div");
      item.className = "project" + (project.id === state.activeProjectId ? " active" : "");
      item.tabIndex = 0;
      item.dataset.projectId = String(project.id);
      item.title = `${project.cwd}\nDrag to reorder\nDouble-click to rename`;

      if (state.editingProjectId === project.id) {
        const input = document.createElement("input");
        input.className = "project-rename";
        input.type = "text";
        input.maxLength = 80;
        input.value = project.name;
        input.setAttribute("aria-label", "Project name");
        let done = false;
        const finish = async (save) => {
          if (done) return;
          done = true;
          const value = input.value.trim();
          state.editingProjectId = null;
          if (save && value && value !== project.name) {
            try {
              await api(`api/projects/${project.id}`, {
                method: "PATCH",
                mutation: true,
                body: { name: value },
              });
            } catch (error) {
              window.setTimeout(() => window.alert(error.message), 0);
            }
          }
          renderProjects();
          scheduleRefresh(0);
        };
        input.addEventListener("keydown", (event) => {
          event.stopPropagation();
          if (event.key === "Enter") {
            event.preventDefault();
            finish(true);
          } else if (event.key === "Escape") {
            event.preventDefault();
            finish(false);
          }
        });
        input.addEventListener("blur", () => finish(true));
        input.addEventListener("click", (event) => event.stopPropagation());
        item.append(input);
        window.setTimeout(() => {
          input.focus();
          input.select();
        }, 0);
      } else {
        const name = document.createElement("span");
        name.className = "project-name";
        name.textContent = project.name;
        item.append(name);

        const close = document.createElement("button");
        close.className = "project-close";
        close.type = "button";
        close.textContent = "x";
        close.title = "Close project";
        close.setAttribute("aria-label", `Close ${project.name}`);
        close.addEventListener("click", (event) => {
          event.stopPropagation();
          closeProject(project);
        });
        item.append(close);

        item.addEventListener("click", () => selectProject(project.id));
        item.addEventListener("dblclick", (event) => {
          event.preventDefault();
          startProjectRename(project);
        });
        item.addEventListener("keydown", (event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            selectProject(project.id);
          }
        });
      }

      if (state.editingProjectId !== project.id) {
        item.draggable = true;
        item.addEventListener("dragstart", (event) => {
          state.draggedProjectId = project.id;
          updateProjectDragIndicator();
          if (event.dataTransfer) {
            event.dataTransfer.effectAllowed = "move";
            event.dataTransfer.setData("text/plain", String(project.id));
          }
        });
        item.addEventListener("dragend", finishProjectDrag);
        item.addEventListener("dragover", (event) => {
          if (state.draggedProjectId === null) return;
          event.preventDefault();
          event.stopPropagation();
          const rect = item.getBoundingClientRect();
          state.projectDropTarget = {
            id: project.id,
            before: event.clientY < rect.top + rect.height / 2,
          };
          if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
          updateProjectDragIndicator();
        });
        item.addEventListener("drop", (event) => {
          if (state.draggedProjectId === null) return;
          event.preventDefault();
          event.stopPropagation();
          const draggedId = state.draggedProjectId;
          const rect = item.getBoundingClientRect();
          const before = event.clientY < rect.top + rect.height / 2;
          finishProjectDrag();
          void reorderProject(draggedId, project.id, before);
        });
      }

      els.projects.append(item);
    }
    updateProjectDragIndicator();
  }

  function updateProjectDragIndicator() {
    const target = state.projectDropTarget;
    for (const item of els.projects.children) {
      const id = Number(item.dataset.projectId);
      item.classList.toggle("dragging", id === state.draggedProjectId);
      item.classList.toggle("drop-before", target?.id === id && target.before);
      item.classList.toggle("drop-after", target?.id === id && !target.before);
    }
    els.projects.classList.toggle("drop-at-end", target?.atEnd === true);
  }

  function finishProjectDrag() {
    state.draggedProjectId = null;
    state.projectDropTarget = null;
    updateProjectDragIndicator();
  }

  async function reorderProject(draggedId, targetId, before) {
    if (draggedId === targetId) return;
    const previousProjects = state.projects;
    if (!previousProjects.some((project) => project.id === draggedId)) return;

    const reorderedProjects = previousProjects.filter((project) => project.id !== draggedId);
    let insertionIndex = reorderedProjects.length;
    if (targetId !== null) {
      const targetIndex = reorderedProjects.findIndex((project) => project.id === targetId);
      if (targetIndex < 0) return;
      insertionIndex = targetIndex + (before ? 0 : 1);
    }
    const dragged = previousProjects.find((project) => project.id === draggedId);
    reorderedProjects.splice(insertionIndex, 0, dragged);
    if (reorderedProjects.every((project, index) => project === previousProjects[index])) return;

    state.projects = reorderedProjects;
    renderProjects();
    try {
      await api("api/projects/reorder", {
        method: "POST",
        mutation: true,
        body: { project_ids: reorderedProjects.map((project) => project.id) },
      });
    } catch (error) {
      if (state.projects === reorderedProjects) {
        state.projects = previousProjects;
        renderProjects();
      }
      if (error.message !== "unauthorized") window.alert(error.message);
    } finally {
      scheduleRefresh(0);
    }
  }

  function selectProject(id) {
    if (state.activeProjectId === id) return;
    state.activeProjectId = id;
    renderProjects();
    renderActiveProject();
    if (els.memoryDialog.open) closeMemoryDialog();
    state.memory.editingDoc = null;
  }

  function startProjectRename(project) {
    state.editingProjectId = project.id;
    renderProjects();
  }

  async function closeProject(project) {
    if (!window.confirm(`Archive "${project.name}"? Its processes stop, but the project is kept in the archive.`)) {
      return;
    }
    try {
      await api(`api/projects/${project.id}`, { method: "DELETE", mutation: true });
    } catch (error) {
      if (error.message !== "unauthorized") window.alert(error.message);
    }
    scheduleRefresh(0);
  }

  function renderActiveProject() {
    const project = state.projects.find((entry) => entry.id === state.activeProjectId) || null;
    els.projectManagement.hidden = !project;
    const projectKey = project ? String(project.id) : null;

    // The memory list lives in this panel now, so it follows the active
    // project instead of waiting for a toggle.
    if (projectKey !== state.memory.loadedFor) {
      state.memory.loadedFor = projectKey;
      if (projectKey) void loadMemoryDocs();
    }

    if (projectKey !== state.renderedProjectKey) {
      disposeHandles();
      state.renderedProjectKey = projectKey;
    }
    if (!project) {
      els.panes.replaceChildren();
      return;
    }

    const paneIds = new Set(project.panes.map((pane) => String(pane.id)));
    for (const [id, handle] of state.handles) {
      if (!paneIds.has(id)) {
        handle.dispose();
        state.handles.delete(id);
      }
    }

    const hasBottom = project.panes.some((pane) => pane.slot.startsWith("bottom"));
    const hasRight = project.panes.some((pane) => pane.slot.endsWith("right"));
    els.panes.style.gridTemplateColumns = `repeat(${hasRight ? 2 : 1}, minmax(0, 1fr))`;
    els.panes.style.gridTemplateRows = `repeat(${hasBottom ? 2 : 1}, minmax(0, 1fr))`;

    for (const pane of project.panes) {
      let handle = state.handles.get(String(pane.id));
      if (!handle) {
        handle = createPane(pane);
        state.handles.set(String(pane.id), handle);
        els.panes.append(handle.el);
        handle.mount();
      }
      const row = !hasBottom || pane.slot.startsWith("top") ? 1 : 2;
      const col = !hasRight || pane.slot.endsWith("left") ? 1 : 2;
      handle.el.style.gridRow = String(row);
      handle.el.style.gridColumn = String(col);
    }

    for (const handle of state.handles.values()) handle.scheduleFit();
  }

  function disposeHandles() {
    for (const handle of state.handles.values()) handle.dispose();
    state.handles.clear();
    els.panes.replaceChildren();
  }

  function createPane(pane) {
    const el = document.createElement("section");
    el.className = "pane";

    const host = document.createElement("div");
    host.className = "pane-term";
    el.append(host);

    const term = new window.Terminal({
      cursorBlink: true,
      fontFamily: TERM_FONT,
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 5000,
      theme: termTheme(),
    });
    const fit = new window.FitAddon.FitAddon();
    term.loadAddon(fit);

    const handle = {
      el,
      host,
      term,
      fit,
      ro: null,
      ws: null,
      retryTimer: null,
      retry: 0,
      disposed: false,
      mounted: false,
      lastSentSize: "",
      fitRaf: 0,
      paneId: pane.id,
      slot: pane.slot,
      queryTail: "",
      pendingResponses: { cpr: 0, status: 0, da: 0 },
      prefixPending: false,
      prefixTimer: null,
      scheduleFit: () => {},
      mount: () => {},
      dispose: () => {},
    };

    const sendFrame = (opcode, bytes) => {
      if (!handle.ws || handle.ws.readyState !== WebSocket.OPEN) return;
      const frame = new Uint8Array(1 + bytes.length);
      frame[0] = opcode;
      frame.set(bytes, 1);
      handle.ws.send(frame);
    };

    term.onData((data) => {
      if (consumeTerminalResponse(handle, data)) return;
      sendFrame(CLIENT_INPUT, encoder.encode(data));
    });
    term.onBinary((data) => {
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i += 1) bytes[i] = data.charCodeAt(i) & 0xff;
      sendFrame(CLIENT_INPUT, bytes);
    });
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown") return true;

      // Browser-reserved shortcuts such as Ctrl+T/Ctrl+W cannot be captured
      // by web content. Use Ctrl+Shift+Space as a prefix, then press the key
      // without Ctrl: prefix + T sends Ctrl+T to the program.
      if (event.ctrlKey && event.shiftKey && !event.altKey && event.code === "Space") {
        event.preventDefault();
        event.stopPropagation();
        handle.prefixPending = !handle.prefixPending;
        window.clearTimeout(handle.prefixTimer);
        if (handle.prefixPending) {
          showToast("Ctrl prefix: press a key");
          handle.prefixTimer = window.setTimeout(() => {
            handle.prefixPending = false;
          }, 2000);
        } else {
          showToast("Ctrl prefix cancelled");
        }
        return false;
      }

      if (handle.prefixPending) {
        event.preventDefault();
        event.stopPropagation();
        handle.prefixPending = false;
        window.clearTimeout(handle.prefixTimer);
        if (event.code !== "Escape" && !event.altKey && !event.metaKey) {
          const byte = controlByte(event);
          if (byte !== null) sendFrame(CLIENT_INPUT, Uint8Array.of(byte));
        }
        return false;
      }

      // Pane focus navigation. Prevent the browser from treating these as
      // text-selection shortcuts and keep the chord away from the program.
      const direction = ARROW_DIRECTIONS[event.code];
      if (event.ctrlKey && event.shiftKey && !event.altKey && direction) {
        event.preventDefault();
        event.stopPropagation();
        focusPaneInDirection(handle, direction);
        return false;
      }

      // Ctrl+Shift+C is reserved by some browsers (DevTools), so also support
      // Ctrl+Insert and Ctrl+C when a selection exists. Plain Ctrl+C with no
      // selection still reaches the program as SIGINT.
      const copyChord =
        (event.ctrlKey && !event.shiftKey && !event.altKey
          && (event.code === "KeyC" || event.code === "Insert"))
        || (event.ctrlKey && event.shiftKey && !event.altKey && event.code === "KeyC");
      if (copyChord && term.hasSelection()) {
        event.preventDefault();
        event.stopPropagation();
        void copyText(term.getSelection());
        return false;
      }

      // Let the browser deliver its native paste event. Returning false only
      // stops xterm from turning the shortcut into terminal input; the event's
      // default action must stay intact. Do not call
      // navigator.clipboard.readText() here: Firefox renders that as a paste
      // context menu instead of pasting directly.
      if (event.ctrlKey && event.shiftKey && event.code === "KeyV") return false;
      if (event.shiftKey && event.code === "Insert") return false;

      return true;
    });

    const sendSize = () => {
      if (!handle.ws || handle.ws.readyState !== WebSocket.OPEN) return;
      const cols = term.cols;
      const rows = term.rows;
      if (cols < 2 || rows < 1) return;
      const size = `${cols}x${rows}`;
      if (size === handle.lastSentSize) return;
      handle.lastSentSize = size;
      const frame = new ArrayBuffer(5);
      const view = new DataView(frame);
      view.setUint8(0, CLIENT_RESIZE);
      view.setUint16(1, cols);
      view.setUint16(3, rows);
      handle.ws.send(frame);
    };

    handle.scheduleFit = () => {
      if (handle.fitRaf || handle.disposed) return;
      handle.fitRaf = window.requestAnimationFrame(() => {
        handle.fitRaf = 0;
        if (handle.disposed || !host.isConnected) return;
        if (host.clientWidth === 0 || host.clientHeight === 0) return;
        try {
          fit.fit();
        } catch {
          return;
        }
        sendSize();
      });
    };

    const connect = () => {
      if (handle.disposed) return;
      window.clearTimeout(handle.retryTimer);
      const base = new URL("./", window.location.href);
      const wsUrl = new URL(`ws/panes/${handle.paneId}`, base);
      wsUrl.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(wsUrl);
      ws.binaryType = "arraybuffer";
      handle.ws = ws;

      ws.onopen = () => {
        if (handle.disposed) {
          ws.close();
          return;
        }
        handle.retry = 0;
        handle.lastSentSize = "";
        handle.scheduleFit();
      };

      ws.onmessage = (event) => {
        if (typeof event.data === "string") return;
        const data = event.data;
        if (data.byteLength < 1) return;
        const view = new DataView(data);
        const opcode = view.getUint8(0);
        if (opcode === SERVER_FULL && data.byteLength >= 5) {
          const rows = view.getUint16(1);
          const cols = view.getUint16(3);
          const payload = new Uint8Array(data, 5);
          term.reset();
          if (rows >= 1 && cols >= 2) term.resize(cols, rows);
          term.write(payload);
        } else if (opcode === SERVER_DATA) {
          const payload = new Uint8Array(data, 1);
          noteTerminalQueries(handle, payload);
          term.write(payload);
        } else if (opcode === SERVER_CLIPBOARD) {
          void copyText(decoder.decode(new Uint8Array(data, 1)));
        }
      };

      ws.onclose = () => {
        handle.ws = null;
        if (handle.disposed) return;
        const delay = Math.min(5000, 400 * 1.7 ** handle.retry);
        handle.retry += 1;
        handle.retryTimer = window.setTimeout(connect, delay);
      };

      ws.onerror = () => {
        ws.close();
      };
    };

    handle.mount = () => {
      if (handle.mounted || handle.disposed) return;
      handle.mounted = true;
      term.open(host);
      handle.ro = new ResizeObserver(() => handle.scheduleFit());
      handle.ro.observe(host);
      connect();
      handle.scheduleFit();
    };

    handle.dispose = () => {
      if (handle.disposed) return;
      handle.disposed = true;
      window.clearTimeout(handle.prefixTimer);
      window.clearTimeout(handle.retryTimer);
      window.cancelAnimationFrame(handle.fitRaf);
      if (handle.ro) handle.ro.disconnect();
      if (handle.ws) handle.ws.close();
      handle.ws = null;
      term.dispose();
      el.remove();
    };

    host.addEventListener("mousedown", () => term.focus());

    return handle;
  }

  function noteTerminalQueries(handle, bytes) {
    const combined = handle.queryTail + latin1Decoder.decode(bytes);
    const carryLength = handle.queryTail.length;
    for (const match of combined.matchAll(TERMINAL_QUERY)) {
      // Ignore matches wholly contained in the previous chunk; they were
      // already counted when that chunk arrived.
      if (match.index + match[0].length <= carryLength) continue;
      if (match[0].endsWith("n")) {
        if (match[0].includes("5n")) handle.pendingResponses.status += 1;
        else handle.pendingResponses.cpr += 1;
      } else {
        handle.pendingResponses.da += 1;
      }
    }
    handle.queryTail = combined.slice(-16);
  }

  function consumeTerminalResponse(handle, data) {
    const pending = handle.pendingResponses;
    if (
      pending.cpr > 0
      && (/^\x1b\[\d+;\d+R$/.test(data) || /^\x1b\[\?\d+;\d+;\d+R$/.test(data))
    ) {
      pending.cpr -= 1;
      return true;
    }
    if (pending.status > 0 && /^\x1b\[\d+n$/.test(data)) {
      pending.status -= 1;
      return true;
    }
    if (
      pending.da > 0
      && (/^\x1b\[\??[\d;]*c$/.test(data) || /^\x1b\[>[\d;]*c$/.test(data))
    ) {
      pending.da -= 1;
      return true;
    }
    return false;
  }

  function controlByte(event) {
    if (event.code === "Space") return 0x00;
    if (event.code === "BracketLeft") return 0x1b;
    if (event.code === "Backslash") return 0x1c;
    if (event.code === "BracketRight") return 0x1d;
    if (event.code === "Digit6" && event.shiftKey) return 0x1e;
    if (event.code === "Minus" && event.shiftKey) return 0x1f;
    if (event.code === "Slash" && event.shiftKey) return 0x7f;
    const letter = /^Key([A-Z])$/.exec(event.code);
    if (letter) return letter[1].charCodeAt(0) - 64;
    return null;
  }

  function activePaneHandle() {
    const pane = document.activeElement && document.activeElement.closest(".pane");
    if (!pane) return null;
    for (const handle of state.handles.values()) {
      if (handle.el === pane) return handle;
    }
    return null;
  }

  function focusPaneInDirection(from, direction) {
    const origin = PANE_GRID[from.slot];
    if (!origin) return;

    // Move along the pressed axis while holding the other coordinate fixed,
    // then wrap to the opposite edge when nothing exists further that way.
    const horizontal = direction === "left" || direction === "right";
    const ahead = direction === "right" || direction === "down";
    const axis = horizontal ? 1 : 0;
    const cross = horizontal ? 0 : 1;

    const candidates = [];
    for (const handle of state.handles.values()) {
      const pos = PANE_GRID[handle.slot];
      if (!pos || pos[cross] !== origin[cross]) continue;
      candidates.push({ handle, offset: pos[axis] - origin[axis] });
    }
    if (candidates.length < 2) return;

    // Nearest pane in the pressed direction first; otherwise the farthest pane
    // on the opposite edge (which sorts first in the wrap order).
    candidates.sort((a, b) => (ahead ? a.offset - b.offset : b.offset - a.offset));
    const target =
      candidates.find((candidate) => (ahead ? candidate.offset > 0 : candidate.offset < 0))
      || candidates[0];
    target.handle.term.focus();
  }

  function basename(path) {
    if (!path) return "";
    const trimmed = path.replace(/\/+$/, "");
    const index = trimmed.lastIndexOf("/");
    return index >= 0 ? trimmed.slice(index + 1) : trimmed;
  }

  async function loadDir(path) {
    const request = ++state.pickerRequest;
    state.pickerLoading = true;
    els.newProjectDialogCreate.disabled = true;
    els.dirCurrent.textContent = "loading...";
    try {
      const query = path ? `?path=${encodeURIComponent(path)}` : "";
      const listing = await api(`api/fs${query}`);
      if (request !== state.pickerRequest) return false;
      els.dirPath.value = listing.path;
      els.dirCurrent.textContent = listing.path;
      els.dirList.replaceChildren();

      if (listing.parent) {
        els.dirList.append(dirRow("..", listing.parent, true));
      }
      for (const entry of listing.entries) {
        els.dirList.append(dirRow(`${entry.name}/`, entry.path, false));
      }
      if (!els.projectName.value) els.projectName.placeholder = basename(listing.path) || "optional";
      els.newProjectDialogError.hidden = true;
      return true;
    } catch (error) {
      if (request !== state.pickerRequest) return false;
      els.dirList.replaceChildren();
      els.dirCurrent.textContent = "";
      showDialogError(error.message);
      return false;
    } finally {
      if (request === state.pickerRequest) {
        state.pickerLoading = false;
        els.newProjectDialogCreate.disabled = false;
      }
    }
  }

  function dirRow(label, path, isUp) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "dir-entry" + (isUp ? " up" : "");
    button.textContent = label;
    button.title = path;
    button.addEventListener("click", () => loadDir(path));
    item.append(button);
    return item;
  }

  function showDialogError(message) {
    els.newProjectDialogError.textContent = message;
    els.newProjectDialogError.hidden = false;
  }

  async function openNewProject() {
    if (els.newProjectDialog.open) return;
    state.pickerRequest += 1;
    els.projectName.value = "";
    els.projectName.placeholder = "";
    els.dirPath.value = "";
    els.dirList.replaceChildren();
    els.dirCurrent.textContent = "";
    els.newProjectDialogError.hidden = true;
    state.pickerLoading = true;
    els.newProjectDialogCreate.disabled = true;
    els.newProjectDialog.showModal();
    const remembered = window.localStorage.getItem("mmux.lastDir");
    const target = remembered || state.root;
    const ok = await loadDir(target);
    if (!ok && target !== state.root) await loadDir(state.root);
    els.projectName.focus();
  }

  els.projects.addEventListener("dragover", (event) => {
    if (state.draggedProjectId === null || event.target !== els.projects) return;
    event.preventDefault();
    state.projectDropTarget = { atEnd: true };
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    updateProjectDragIndicator();
  });
  els.projects.addEventListener("drop", (event) => {
    if (state.draggedProjectId === null || event.target !== els.projects) return;
    event.preventDefault();
    const draggedId = state.draggedProjectId;
    finishProjectDrag();
    void reorderProject(draggedId, null, false);
  });
  els.projects.addEventListener("dragend", finishProjectDrag);

  els.newProject.addEventListener("click", openNewProject);
  els.emptyNewProject.addEventListener("click", openNewProject);
  els.newProjectDialogCancel.addEventListener("click", () => els.newProjectDialog.close());

  els.dirPath.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      const value = els.dirPath.value.trim();
      if (value) loadDir(value);
    }
  });

  els.newProjectForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (state.pickerLoading) {
      showDialogError("Still loading this directory.");
      return;
    }
    const cwd = els.dirPath.value.trim();
    if (!cwd) {
      showDialogError("Choose or enter a directory first.");
      return;
    }
    els.newProjectDialogCreate.disabled = true;
    try {
      const name = els.projectName.value.trim();
      const project = await api("api/projects", {
        method: "POST",
        mutation: true,
        body: { name: name || null, cwd },
      });
      window.localStorage.setItem("mmux.lastDir", project.cwd);
      els.newProjectDialog.close();
      state.activeProjectId = project.id;
      await refresh();
    } catch (error) {
      showDialogError(error.message);
    } finally {
      els.newProjectDialogCreate.disabled = false;
    }
  });

  // Project memory drawer and archive panel are opt-in: nothing is fetched
  // until their toggle is pressed, except the archive badge count.

  function memoryWorkspaceQuery() {
    const project = state.projects.find((entry) => entry.id === state.activeProjectId);
    return project && project.cwd ? project.cwd : "";
  }

  function withMemoryWorkspace(path) {
    const workspace = memoryWorkspaceQuery();
    if (!workspace) return path;
    const separator = path.includes("?") ? "&" : "?";
    return `${path}${separator}workspace=${encodeURIComponent(workspace)}`;
  }

  function showMemoryEditorError(message) {
    els.memoryEditorError.textContent = message;
    els.memoryEditorError.hidden = false;
  }

  function showMemoryListError(message) {
    els.memoryListError.textContent = message;
    els.memoryListError.hidden = false;
  }

  function renderMemoryStatus() {
    const status = state.memory.status;
    // A healthy or irrelevant server says nothing; this box exists only to
    // report a problem.
    if (
      !status ||
      status.state === "ready" ||
      status.state === "not_applicable"
    ) {
      els.memoryStatus.hidden = true;
      els.memoryStatusRecheck.hidden = true;
      return;
    }
    els.memoryStatus.hidden = false;
    els.memoryStatus.classList.add("warning");
    els.memoryStatusMessage.textContent =
      status.message || `project memory is ${status.state}.`;
    const advice = (status.advice || "").trim();
    els.memoryStatusAdvice.textContent = advice;
    els.memoryStatusAdvice.hidden = advice === "";
    els.memoryStatusRecheck.hidden = false;
  }

  async function loadMemoryStatus(refresh = false) {
    try {
      state.memory.status = await api(
        refresh ? "api/memory/status?refresh=1" : "api/memory/status",
      );
    } catch (error) {
      if (error.message === "unauthorized") return;
      state.memory.status = {
        enabled: false,
        state: "invalid",
        message: error.message,
        advice: null,
      };
    }
    renderMemoryStatus();
  }

  async function loadMemoryDocs() {
    try {
      const data = await api(withMemoryWorkspace("api/memory/docs"));
      state.memory.docs = data && Array.isArray(data.docs) ? data.docs : [];
      els.memoryListError.hidden = true;
      renderMemoryList();
    } catch (error) {
      if (error.message === "unauthorized") return;
      state.memory.docs = [];
      state.memory.hits = [];
      els.memoryList.replaceChildren();
      showMemoryListError(error.message);
    }
  }

  /// `YYYY-MM-DD HH:MM` in the browser's local timezone.
  function formatMemoryDate(value) {
    if (!value) return "";
    // Trim sub-millisecond precision so every browser parses the ISO string.
    const normalized = String(value).replace(/\.(\d{3})\d+/, ".$1");
    const date = new Date(normalized);
    if (Number.isNaN(date.getTime())) return "";
    const pad = (number) => String(number).padStart(2, "0");
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
  }

  /// Docs rows are two lines: the title, then a dim `kind` and date.
  function memoryDocRow(doc, onOpen, dateValue) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "memory-item";

    const title = document.createElement("span");
    title.className = "memory-item-title";
    title.textContent = doc.title || doc.slug;
    button.append(title);

    const meta = document.createElement("span");
    meta.className = "memory-item-meta";
    const kind = document.createElement("span");
    kind.textContent = doc.kind;
    const date = document.createElement("span");
    date.className = "memory-item-date";
    date.textContent = formatMemoryDate(dateValue || doc.updated);
    meta.append(kind, date);
    button.append(meta);

    button.addEventListener("click", () => {
      if (onOpen) {
        onOpen();
      } else {
        void openMemoryDoc(doc.slug);
      }
    });
    item.append(button);
    return item;
  }

  function memoryMetaLine(parts) {
    return parts.filter((part) => part !== null && part !== undefined && part !== "").join(" - ");
  }

  function memoryEmptyRow(message) {
    const item = document.createElement("li");
    item.className = "memory-empty";
    item.textContent = message;
    return item;
  }

  function memoryItem(parts) {
    const { title, meta, snippet, onOpen } = parts;
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "memory-item";

    const titleEl = document.createElement("span");
    titleEl.className = "memory-item-title";
    titleEl.textContent = title;
    button.append(titleEl);

    if (snippet) {
      const snippetEl = document.createElement("span");
      snippetEl.className = "memory-item-snippet";
      snippetEl.textContent = snippet;
      button.append(snippetEl);
    }

    const metaEl = document.createElement("span");
    metaEl.className = "memory-item-meta";
    metaEl.textContent = memoryMetaLine(meta);
    button.append(metaEl);

    button.addEventListener("click", onOpen);
    item.append(button);
    return item;
  }

  function renderMemoryList() {
    els.memoryList.replaceChildren();
    if (state.memory.query) {
      if (!state.memory.hits.length) {
        els.memoryList.append(memoryEmptyRow("no matches"));
        return;
      }
      for (const hit of state.memory.hits) {
        els.memoryList.append(memoryItem({
          title: hit.title || hit.slug,
          meta: [hit.kind],
          snippet: hit.snippet,
          onOpen: () => {
            void openMemoryDoc(hit.slug);
          },
        }));
      }
      return;
    }
    if (!state.memory.docs.length) {
      els.memoryList.append(memoryEmptyRow("no docs yet"));
      return;
    }
    for (const doc of state.memory.docs) {
      els.memoryList.append(memoryDocRow(doc));
    }
  }

  async function openMemoryArchive() {
    if (els.memoryArchiveDialog.open) return;
    els.memoryArchiveDialog.showModal();
    renderMemoryArchive();
    await loadMemoryArchive();
  }

  function closeMemoryArchive() {
    if (els.memoryArchiveDialog.open) els.memoryArchiveDialog.close();
  }

  async function loadMemoryArchive() {
    try {
      const data = await api(withMemoryWorkspace("api/memory/archive"));
      state.memory.archived = data && Array.isArray(data.docs) ? data.docs : [];
      els.memoryArchiveError.hidden = true;
      els.memoryArchiveRetry.hidden = true;
      els.memoryArchiveEmpty.hidden = state.memory.archived.length > 0;
      renderMemoryArchive();
    } catch (error) {
      if (error.message === "unauthorized") return;
      state.memory.archived = [];
      renderMemoryArchive();
      els.memoryArchiveEmpty.hidden = true;
      els.memoryArchiveError.textContent = error.message;
      els.memoryArchiveError.hidden = false;
      els.memoryArchiveRetry.hidden = false;
    }
  }

  function renderMemoryArchive() {
    els.memoryArchiveList.replaceChildren();
    for (const doc of state.memory.archived) {
      els.memoryArchiveList.append(memoryArchiveRow(doc));
    }
  }

  function memoryArchiveRow(doc) {
    const item = document.createElement("li");
    item.className = "archive-item";

    const head = document.createElement("div");
    head.className = "archive-item-head";

    const title = document.createElement("button");
    title.type = "button";
    title.className = "archive-item-name archive-item-open";
    title.textContent = doc.title || doc.slug;
    title.title = "view this archived doc";
    title.addEventListener("click", () => {
      void openArchivedDoc(doc.id, doc);
    });

    const date = document.createElement("span");
    date.className = "archive-item-date";
    date.textContent = formatMemoryDate(doc.archived_at);

    head.append(title, date);

    const meta = document.createElement("div");
    meta.className = "archive-item-meta";
    meta.textContent = doc.kind;

    const actions = document.createElement("div");
    actions.className = "archive-item-actions";

    const restore = document.createElement("button");
    restore.type = "button";
    restore.className = "archive-action";
    restore.textContent = "restore";
    restore.addEventListener("click", () => {
      void restoreArchivedDoc(doc, restore);
    });

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "archive-action danger";
    remove.textContent = "delete permanently";

    const confirmArea = memoryArchiveConfirm(doc, remove);
    remove.addEventListener("click", () => {
      confirmArea.hidden = false;
      const input = confirmArea.querySelector(".archive-confirm-input");
      if (input) input.focus();
    });

    actions.append(restore, remove);
    item.append(head, meta, actions, confirmArea);
    return item;
  }

  function memoryArchiveConfirm(doc, trigger) {
    const wrap = document.createElement("div");
    wrap.className = "archive-confirm";
    wrap.hidden = true;

    const hint = document.createElement("span");
    hint.className = "archive-confirm-hint";
    hint.textContent = `type "${doc.slug}" to confirm permanent deletion`;

    const input = document.createElement("input");
    input.type = "text";
    input.className = "archive-confirm-input";
    input.placeholder = "type the doc slug to confirm";
    input.autocomplete = "off";
    input.spellcheck = false;
    input.setAttribute("aria-label", `Type ${doc.slug} to confirm deletion`);

    const confirm = document.createElement("button");
    confirm.type = "button";
    confirm.className = "archive-action danger";
    confirm.textContent = "confirm delete";
    confirm.disabled = true;

    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "archive-action";
    cancel.textContent = "cancel";

    input.addEventListener("input", () => {
      confirm.disabled = input.value !== doc.slug;
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !confirm.disabled) {
        event.preventDefault();
        void purgeArchivedDoc(doc, confirm, input);
      }
    });
    cancel.addEventListener("click", () => {
      wrap.hidden = true;
      input.value = "";
      confirm.disabled = true;
      trigger.focus();
    });
    confirm.addEventListener("click", () => {
      void purgeArchivedDoc(doc, confirm, input);
    });

    wrap.append(hint, input, confirm, cancel);
    return wrap;
  }

  async function restoreArchivedDoc(doc, button) {
    button.disabled = true;
    try {
      await api(
        withMemoryWorkspace(`api/memory/archive/${encodeURIComponent(doc.id)}/restore`),
        { method: "POST", mutation: true },
      );
      showToast(`Restored ${doc.title || doc.slug}`);
      await loadMemoryArchive();
      void loadMemoryDocs();
    } catch (error) {
      button.disabled = false;
      if (error.message !== "unauthorized") showToast(`could not restore: ${error.message}`);
    }
  }

  async function purgeArchivedDoc(doc, button, input) {
    button.disabled = true;
    try {
      await api(
        withMemoryWorkspace(`api/memory/archive/${encodeURIComponent(doc.id)}`),
        { method: "DELETE", mutation: true },
      );
      showToast("Archived doc deleted");
      await loadMemoryArchive();
    } catch (error) {
      button.disabled = input.value !== doc.slug;
      if (error.message !== "unauthorized") showToast(`could not delete: ${error.message}`);
    }
  }

  async function openArchivedDoc(id, summary) {
    try {
      const doc = await api(
        withMemoryWorkspace(`api/memory/archive/${encodeURIComponent(id)}`),
      );
      openArchivedEditor(doc || summary);
    } catch (error) {
      if (error.message !== "unauthorized") showToast(`could not open archived doc: ${error.message}`);
    }
  }

  function openArchivedEditor(doc) {
    state.memory.editingDoc = null;
    state.memory.editingWorkspace = null;
    els.memoryDialogTitle.textContent = "archived doc";
    els.memoryTitle.value = doc.title || "";
    els.memoryKind.value = MEMORY_KINDS.includes(doc.kind) ? doc.kind : "note";
    els.memoryTags.value = Array.isArray(doc.tags) ? doc.tags.join(", ") : "";
    els.memoryBody.value = doc.body || "";
    for (const field of [els.memoryTitle, els.memoryKind, els.memoryTags]) {
      field.disabled = true;
    }
    els.memoryBody.readOnly = true;
    els.memoryEditorError.hidden = true;
    els.memoryReload.hidden = true;
    els.memoryDelete.hidden = true;
    els.memorySave.hidden = true;
    els.memoryCancel.textContent = "close";
    if (!els.memoryDialog.open) els.memoryDialog.showModal();
    els.memoryCancel.focus();
  }

  async function openMemoryDoc(slug, workspace) {
    if (!slug) return;
    state.memory.editingWorkspace = workspace || null;
    try {
      const doc = await api(
        withEditingWorkspace(`api/memory/docs/${encodeURIComponent(slug)}`),
      );
      openMemoryEditor(doc);
    } catch (error) {
      if (error.message !== "unauthorized") showToast(`could not open doc: ${error.message}`);
    }
  }

  function openMemoryEditor(doc) {
    state.memory.editingDoc = doc || null;
    if (!doc) state.memory.editingWorkspace = null;
    for (const field of [els.memoryTitle, els.memoryKind, els.memoryTags]) {
      field.disabled = false;
    }
    els.memoryBody.readOnly = false;
    els.memorySave.hidden = false;
    els.memoryCancel.textContent = "cancel";
    els.memoryDialogTitle.textContent = doc ? "edit doc" : "new doc";
    els.memoryTitle.value = doc ? doc.title || "" : "";
    els.memoryKind.value = doc && MEMORY_KINDS.includes(doc.kind) ? doc.kind : "note";
    els.memoryTags.value = doc && Array.isArray(doc.tags) ? doc.tags.join(", ") : "";
    els.memoryBody.value = doc ? doc.body || "" : "";
    els.memoryEditorError.hidden = true;
    els.memoryReload.hidden = true;
    els.memoryDelete.hidden = !doc;
    if (!els.memoryDialog.open) els.memoryDialog.showModal();
    els.memoryTitle.focus();
  }

  function closeMemoryEditor() {
    state.memory.editingDoc = null;
    state.memory.editingWorkspace = null;
    els.memoryEditorError.hidden = true;
    els.memoryReload.hidden = true;
    els.memoryDelete.hidden = true;
    els.memorySave.hidden = false;
    els.memoryCancel.textContent = "cancel";
    for (const field of [els.memoryTitle, els.memoryKind, els.memoryTags]) {
      field.disabled = false;
    }
    els.memoryBody.readOnly = false;
    if (els.memoryDialog.open) els.memoryDialog.close();
  }

  function parseMemoryTags(value) {
    return value
      .split(",")
      .map((tag) => tag.trim())
      .filter((tag) => tag.length > 0);
  }

  async function saveMemoryDoc() {
    const title = els.memoryTitle.value.trim();
    if (!title) {
      showMemoryEditorError("Add a title first.");
      return;
    }
    const body = els.memoryBody.value;
    const kind = els.memoryKind.value;
    const tags = parseMemoryTags(els.memoryTags.value);
    const editing = state.memory.editingDoc;
    els.memorySave.disabled = true;
    els.memoryEditorError.hidden = true;
    const workspace = state.memory.editingWorkspace || memoryWorkspaceQuery();
    try {
      if (editing) {
        await api(withEditingWorkspace(`api/memory/docs/${encodeURIComponent(editing.slug)}`), {
          method: "PUT",
          mutation: true,
          headers: editing.hash ? { "If-Match": editing.hash } : undefined,
          body: { body, title, kind, tags, workspace },
        });
      } else {
        await api("api/memory/docs", {
          method: "POST",
          mutation: true,
          body: { title, kind, body, tags, workspace },
        });
      }
      closeMemoryEditor();
      showToast("Saved to project memory");
      void loadMemoryDocs();
    } catch (error) {
      if (error.status === 409) {
        showMemoryEditorError("someone else changed this doc; copy your text, then reload");
        els.memoryReload.hidden = false;
      } else if (error.message !== "unauthorized") {
        showMemoryEditorError(error.message);
      }
    } finally {
      els.memorySave.disabled = false;
    }
  }

  async function reloadMemoryDoc() {
    const editing = state.memory.editingDoc;
    if (!editing) return;
    try {
      const doc = await api(
        withEditingWorkspace(`api/memory/docs/${encodeURIComponent(editing.slug)}`),
      );
      openMemoryEditor(doc);
      showToast("Reloaded doc from server");
      void loadMemoryDocs();
    } catch (error) {
      if (error.message !== "unauthorized") showMemoryEditorError(error.message);
    }
  }

  async function deleteMemoryDoc() {
    const editing = state.memory.editingDoc;
    if (!editing) return;
    const label = editing.title || editing.slug;
    if (!window.confirm(`Archive "${label}"? It moves to the project archive.`)) return;
    els.memoryDelete.disabled = true;
    try {
      await api(withEditingWorkspace(`api/memory/docs/${encodeURIComponent(editing.slug)}`), {
        method: "DELETE",
        mutation: true,
      });
      closeMemoryEditor();
      showToast("Doc archived");
      void loadMemoryDocs();
      void loadMemoryArchive();
    } catch (error) {
      if (error.message !== "unauthorized") showMemoryEditorError(error.message);
    } finally {
      els.memoryDelete.disabled = false;
    }
  }

  function scheduleMemorySearch() {
    window.clearTimeout(state.memory.searchTimer);
    state.memory.query = els.memorySearch.value.trim();
    if (!state.memory.query) {
      state.memory.hits = [];
      state.memory.searchToken += 1;
      els.memoryListError.hidden = true;
      renderMemoryList();
      return;
    }
    state.memory.searchTimer = window.setTimeout(() => {
      void runMemorySearch();
    }, 200);
  }

  async function runMemorySearch() {
    const query = state.memory.query;
    if (!query) return;
    const token = ++state.memory.searchToken;
    try {
      const data = await api(
        withMemoryWorkspace(`api/memory/search?q=${encodeURIComponent(query)}`),
      );
      if (token !== state.memory.searchToken) return;
      state.memory.hits = data && Array.isArray(data.hits) ? data.hits : [];
      els.memoryListError.hidden = true;
      renderMemoryList();
    } catch (error) {
      if (token !== state.memory.searchToken || error.message === "unauthorized") return;
      state.memory.hits = [];
      els.memoryList.replaceChildren();
      showMemoryListError(error.message);
    }
  }

  function withEditingWorkspace(path) {
    const workspace = state.memory.editingWorkspace;
    if (!workspace) return withMemoryWorkspace(path);
    const separator = path.includes("?") ? "&" : "?";
    return `${path}${separator}workspace=${encodeURIComponent(workspace)}`;
  }

  function closeMemoryDialog() {
    closeMemoryEditor();
  }

  function countLabel(count, noun) {
    const value = Number(count) || 0;
    return `${value} ${noun}${value === 1 ? "" : "s"}`;
  }

  function middleElide(text, max = 46) {
    if (!text || text.length <= max) return text || "";
    const tail = Math.ceil((max - 1) / 2);
    const head = max - 1 - tail;
    return `${text.slice(0, head)}...${text.slice(text.length - tail)}`;
  }

  function formatArchiveDate(value) {
    if (!value) return "";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? String(value) : date.toLocaleString();
  }

  function archiveEntries(data) {
    return data && data.readable && Array.isArray(data.entries) ? data.entries : [];
  }

  function updateArchiveToggle() {
    const count = state.archive.count;
    els.archiveToggle.title = count > 0 ? `archived projects (${count})` : "archived projects";
  }

  async function fetchArchive() {
    try {
      state.archive.data = await api("api/archive");
      state.archive.error = null;
    } catch (error) {
      if (error.message === "unauthorized") return;
      state.archive.data = null;
      state.archive.error = error.message;
    }
    state.archive.count = archiveEntries(state.archive.data).length;
    updateArchiveToggle();
  }

  function forgetArchivedEntry(id) {
    const remaining = archiveEntries(state.archive.data).filter((entry) => entry.id !== id);
    if (state.archive.data) state.archive.data.entries = remaining;
    state.archive.count = remaining.length;
    updateArchiveToggle();
  }

  async function restoreArchivedProject(entry, button) {
    button.disabled = true;
    try {
      await api(`api/archive/${encodeURIComponent(entry.id)}/restore`, {
        method: "POST",
        mutation: true,
      });
      closeArchiveDialog();
      showToast(`Restored ${entry.name}`);
      await refresh();
      await fetchArchive();
    } catch (error) {
      button.disabled = false;
      if (error.message !== "unauthorized") window.alert(error.message);
    }
  }

  async function deleteArchivedProject(entry, button, input) {
    button.disabled = true;
    try {
      await api(`api/archive/${encodeURIComponent(entry.id)}`, {
        method: "DELETE",
        mutation: true,
      });
      forgetArchivedEntry(entry.id);
      showToast("Project deleted permanently");
      renderArchive();
    } catch (error) {
      button.disabled = input.value !== entry.name;
      if (error.message !== "unauthorized") window.alert(error.message);
    }
  }

  function archiveConfirmArea(entry, trigger) {
    const wrap = document.createElement("div");
    wrap.className = "archive-confirm";
    wrap.hidden = true;

    const hint = document.createElement("span");
    hint.className = "archive-confirm-hint";
    hint.textContent = `type "${entry.name}" to confirm permanent deletion`;

    const input = document.createElement("input");
    input.type = "text";
    input.className = "archive-confirm-input";
    input.placeholder = "type the project name to confirm";
    input.autocomplete = "off";
    input.spellcheck = false;
    input.setAttribute("aria-label", `Type ${entry.name} to confirm deletion`);

    const confirm = document.createElement("button");
    confirm.type = "button";
    confirm.className = "archive-action danger";
    confirm.textContent = "confirm delete";
    confirm.disabled = true;

    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "archive-action";
    cancel.textContent = "cancel";

    input.addEventListener("input", () => {
      confirm.disabled = input.value !== entry.name;
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !confirm.disabled) {
        event.preventDefault();
        void deleteArchivedProject(entry, confirm, input);
      }
    });
    cancel.addEventListener("click", () => {
      wrap.hidden = true;
      input.value = "";
      confirm.disabled = true;
      trigger.focus();
    });
    confirm.addEventListener("click", () => {
      void deleteArchivedProject(entry, confirm, input);
    });

    wrap.append(hint, input, confirm, cancel);
    return wrap;
  }

  function archiveRow(entry) {
    const item = document.createElement("li");
    item.className = "archive-item";

    const head = document.createElement("div");
    head.className = "archive-item-head";

    const name = document.createElement("span");
    name.className = "archive-item-name";
    name.textContent = entry.name;
    name.title = entry.name;

    const date = document.createElement("span");
    date.className = "archive-item-date";
    date.textContent = formatArchiveDate(entry.archived_at);

    head.append(name, date);

    const cwd = document.createElement("div");
    cwd.className = "archive-item-cwd";
    cwd.textContent = middleElide(entry.cwd);
    cwd.title = entry.cwd || "";

    const meta = document.createElement("div");
    meta.className = "archive-item-meta";
    const parts = [countLabel(entry.pane_count, "pane")];
    if (entry.memory_docs !== null && entry.memory_docs !== undefined) {
      parts.push(countLabel(entry.memory_docs, "doc"));
    }
    meta.textContent = parts.join(", ");

    const actions = document.createElement("div");
    actions.className = "archive-item-actions";

    const restore = document.createElement("button");
    restore.type = "button";
    restore.className = "archive-action";
    restore.textContent = "restore";
    restore.addEventListener("click", () => {
      void restoreArchivedProject(entry, restore);
    });

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "archive-action danger";
    remove.textContent = "delete permanently";

    const confirmArea = archiveConfirmArea(entry, remove);
    remove.addEventListener("click", () => {
      confirmArea.hidden = false;
      const input = confirmArea.querySelector(".archive-confirm-input");
      if (input) input.focus();
    });

    actions.append(restore, remove);
    item.append(head, cwd, meta, actions, confirmArea);
    return item;
  }

  function showArchiveError(message) {
    els.archiveError.textContent = message;
    els.archiveError.hidden = false;
  }

  function renderArchive() {
    els.archiveList.replaceChildren();
    els.archiveRetry.hidden = true;

    const data = state.archive.data;
    if (!data) {
      if (state.archive.error) {
        els.archiveEmpty.hidden = true;
        showArchiveError(`The archive index could not be read. ${state.archive.error}`);
        els.archiveRetry.hidden = false;
      } else {
        els.archiveError.hidden = true;
        els.archiveEmpty.textContent = "loading...";
        els.archiveEmpty.hidden = false;
      }
      return;
    }

    if (!data.readable) {
      els.archiveEmpty.hidden = true;
      const reason = data.error ? `: ${data.error}` : "";
      showArchiveError(`The archive index could not be read${reason}`);
      els.archiveRetry.hidden = false;
      return;
    }

    els.archiveError.hidden = true;
    const entries = archiveEntries(data);
    if (!entries.length) {
      els.archiveEmpty.textContent = "No archived projects";
      els.archiveEmpty.hidden = false;
      return;
    }
    els.archiveEmpty.hidden = true;
    for (const entry of entries) els.archiveList.append(archiveRow(entry));
  }

  async function openArchiveDialog() {
    if (els.archiveDialog.open) return;
    els.archiveDialog.showModal();
    renderArchive();
    await fetchArchive();
    renderArchive();
  }

  function closeArchiveDialog() {
    if (els.archiveDialog.open) els.archiveDialog.close();
  }

  els.archiveToggle.addEventListener("click", () => {
    void openArchiveDialog();
  });
  els.memoryClose.addEventListener("click", closeMemoryDialog);
  els.archiveClose.addEventListener("click", closeArchiveDialog);
  els.memoryDialog.addEventListener("close", () => {
    state.memory.editingDoc = null;
    els.memoryEditorError.hidden = true;
    if (!els.projectManagement.hidden) els.memoryNew.focus();
  });
  els.archiveDialog.addEventListener("close", () => {
    els.archiveToggle.focus();
  });
  els.memoryStatusRecheck.addEventListener("click", () => {
    void loadMemoryStatus(true);
  });
  els.memorySearch.addEventListener("input", scheduleMemorySearch);
  els.memoryArchiveToggle.addEventListener("click", () => {
    void openMemoryArchive();
  });
  els.memoryArchiveClose.addEventListener("click", closeMemoryArchive);
  els.memoryArchiveRetry.addEventListener("click", () => {
    state.memory.archived = [];
    void loadMemoryArchive();
  });
  els.memoryArchiveDialog.addEventListener("close", () => {
    els.memoryArchiveToggle.focus();
  });
  els.memoryNew.addEventListener("click", () => openMemoryEditor(null));
  els.memorySave.addEventListener("click", () => {
    void saveMemoryDoc();
  });
  els.memoryReload.addEventListener("click", () => {
    void reloadMemoryDoc();
  });
  els.memoryDelete.addEventListener("click", () => {
    void deleteMemoryDoc();
  });
  els.memoryCancel.addEventListener("click", closeMemoryEditor);
  els.archiveRetry.addEventListener("click", async () => {
    state.archive.data = null;
    state.archive.error = null;
    renderArchive();
    await fetchArchive();
    renderArchive();
  });

  els.logout.addEventListener("click", async () => {
    try {
      await api("logout", { method: "POST", mutation: true });
    } catch {
      // The redirect below is the desired end state either way.
    }
    window.location.replace("login");
  });

  // Firefox focuses/opens its menu bar on the Alt *keyup*, which swallows the
  // chord that follows: Alt+X never reaches xterm, so emacs M-x is impossible.
  // Cancelling that one event keeps focus in the terminal. The Alt *keydown* is
  // deliberately left alone: when it is cancelled some Firefox/GTK builds drop
  // the modifier for the next key, so xterm sees altKey=false and sends a plain
  // "x" instead of ESC+x. Macs are exempt because Option is a compose key
  // there, and AltGr must keep producing characters on Windows/Linux layouts
  // that use it.
  const APPLE = /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent);
  const keepAltInTerminal = (event) => {
    if (APPLE || event.ctrlKey || event.metaKey) return;
    if (event.key !== "Alt") return;
    if (event.getModifierState && event.getModifierState("AltGraph")) return;
    if (!activePaneHandle()) return;
    event.preventDefault();
  };
  document.addEventListener("keyup", keepAltInTerminal, true);

  // Firefox reserves Ctrl+Shift+C for DevTools. Handle it in the capture phase
  // and prevent the browser default before xterm sees the key event.
  document.addEventListener("keydown", (event) => {
    if (
      event.type !== "keydown"
      || !event.ctrlKey
      || !event.shiftKey
      || event.altKey
      || event.code !== "KeyC"
    ) {
      return;
    }
    const handle = activePaneHandle();
    if (!handle || !handle.term.hasSelection()) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    void copyText(handle.term.getSelection());
  }, true);

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && state.events) scheduleRefresh(0);
  });

  refresh();
  // Two small eager fetches, both needed for the sidebar badges and warning
  // marker: the archived-projects count, and the memory readiness verdict.
  // The drawer's document list still stays untouched until it is opened.
  void fetchArchive();
  void loadMemoryStatus();
})();
