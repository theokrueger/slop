(() => {
  "use strict";

  const TERM_FONT = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';
  const CLIENT_RESIZE = 0;
  const CLIENT_INPUT = 1;
  const SERVER_FULL = 0;
  const SERVER_CLIPBOARD = 2;
  const SERVER_DATA = 3;

  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");
  const decoder = new TextDecoder();
  const latin1Decoder = new TextDecoder("latin1");
  // Only queries that mmux answers server-side need their xterm auto-response
  // suppressed. This avoids duplicate replies reaching the program.
  const TERMINAL_QUERY = /\x1b\[(?:5n|6n|\?6n|(?:0)?c|>0?c)/g;

  const PANE_NEIGHBORS = {
    "top-left": { up: null, down: "bottom-left", left: null, right: "top-right" },
    "top-right": { up: null, down: "bottom-right", left: "top-left", right: null },
    "bottom-left": { up: "top-left", down: null, left: null, right: "bottom-right" },
    "bottom-right": { up: "top-right", down: null, left: "bottom-left", right: null },
  };
  const ARROW_DIRECTIONS = {
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
  };

  const els = {
    loading: document.getElementById("loading"),
    app: document.getElementById("app"),
    projects: document.getElementById("projects"),
    newProject: document.getElementById("new-project"),
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
    activeProjectId: null,
    user: null,
    root: null,
    handles: new Map(),
    renderedProjectKey: null,
    editingProjectId: null,
    refreshTimer: null,
    refreshing: false,
    pendingRefresh: false,
    events: null,
    pickerRequest: 0,
    pickerLoading: false,
  };

  const encoder = new TextEncoder();

  async function api(path, { method = "GET", body, mutation = false } = {}) {
    const headers = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (mutation) headers["X-MMUX"] = "1";
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
      throw new Error(message);
    }
    return data;
  }

  function termTheme() {
    if (darkQuery.matches) {
      return {
        background: "#0d0f12",
        foreground: "#d8dde4",
        cursor: "#d8dde4",
        selectionBackground: "#2c3a4d",
        black: "#3b414b",
        brightBlack: "#6b7280",
        white: "#d8dde4",
        brightWhite: "#ffffff",
      };
    }
    return {
      background: "#ffffff",
      foreground: "#1b1f24",
      cursor: "#1b1f24",
      selectionBackground: "#d9e4f5",
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

  function scheduleRefresh(delay = 60) {
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = window.setTimeout(refresh, delay);
  }

  function connectEvents() {
    if (state.events) return;
    state.events = new EventSource("api/events");
    state.events.onmessage = () => scheduleRefresh();
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
      item.title = `${project.cwd}\nDouble-click to rename`;

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
        close.textContent = "×";
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

      els.projects.append(item);
    }
  }

  function selectProject(id) {
    if (state.activeProjectId === id) return;
    state.activeProjectId = id;
    renderProjects();
    renderActiveProject();
  }

  function startProjectRename(project) {
    state.editingProjectId = project.id;
    renderProjects();
  }

  async function closeProject(project) {
    if (!window.confirm(`Close “${project.name}”? All processes in this project will be terminated.`)) {
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
    const projectKey = project ? String(project.id) : null;

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

  function activePaneHandle() {
    const pane = document.activeElement && document.activeElement.closest(".pane");
    if (!pane) return null;
    for (const handle of state.handles.values()) {
      if (handle.el === pane) return handle;
    }
    return null;
  }

  function focusPaneInDirection(from, direction) {
    const targetSlot = PANE_NEIGHBORS[from.slot]?.[direction];
    if (!targetSlot) return;
    for (const handle of state.handles.values()) {
      if (handle.slot === targetSlot) {
        handle.term.focus();
        return;
      }
    }
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
    els.dirCurrent.textContent = "loading…";
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

  els.logout.addEventListener("click", async () => {
    try {
      await api("logout", { method: "POST", mutation: true });
    } catch {
      // The redirect below is the desired end state either way.
    }
    window.location.replace("login");
  });

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

  darkQuery.addEventListener("change", () => {
    const theme = termTheme();
    for (const handle of state.handles.values()) handle.term.options.theme = theme;
  });

  refresh();
})();
