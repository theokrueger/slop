(() => {
  "use strict";

  const TERM_FONT = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';
  const CLIENT_RESIZE = 0;
  const CLIENT_INPUT = 1;
  const SERVER_FULL = 0;
  const SERVER_DIFF = 1;

  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  const els = {
    loading: document.getElementById("loading"),
    app: document.getElementById("app"),
    tabs: document.getElementById("tabs"),
    newTab: document.getElementById("new-tab"),
    user: document.getElementById("user"),
    logout: document.getElementById("logout"),
    main: document.getElementById("main"),
    panes: document.getElementById("panes"),
    emptyState: document.getElementById("empty-state"),
    emptyNewTab: document.getElementById("empty-new-tab"),
    dialog: document.getElementById("new-tab-dialog"),
    form: document.getElementById("new-tab-form"),
    name: document.getElementById("new-tab-name"),
    dirPath: document.getElementById("dir-path"),
    dirCurrent: document.getElementById("dir-current"),
    dirList: document.getElementById("dir-list"),
    dialogError: document.getElementById("dialog-error"),
    dialogCancel: document.getElementById("dialog-cancel"),
    dialogCreate: document.getElementById("dialog-create"),
  };

  const state = {
    tabs: [],
    activeId: null,
    user: null,
    root: null,
    handles: new Map(),
    renderedTabKey: null,
    editingTabId: null,
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

  async function refresh() {
    if (state.refreshing) {
      state.pendingRefresh = true;
      return;
    }
    state.refreshing = true;
    try {
      const data = await api("api/state");
      state.tabs = data.tabs;
      state.user = data.user;
      state.root = data.workspace_root;
      if (!state.tabs.some((tab) => tab.id === state.activeId)) {
        state.activeId = state.tabs.length ? state.tabs[0].id : null;
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
    if (!state.editingTabId) renderTabs();
    renderActiveTab();
    els.emptyState.hidden = state.tabs.length !== 0;
    els.panes.hidden = state.tabs.length === 0;
  }

  function renderTabs() {
    els.tabs.replaceChildren();
    for (const tab of state.tabs) {
      const item = document.createElement("div");
      item.className = "tab" + (tab.id === state.activeId ? " active" : "");
      item.tabIndex = 0;
      item.title = `${tab.cwd}\nDouble-click to rename`;

      if (state.editingTabId === tab.id) {
        const input = document.createElement("input");
        input.className = "tab-rename";
        input.type = "text";
        input.maxLength = 80;
        input.value = tab.name;
        input.setAttribute("aria-label", "Tab name");
        let done = false;
        const finish = async (save) => {
          if (done) return;
          done = true;
          const value = input.value.trim();
          state.editingTabId = null;
          if (save && value && value !== tab.name) {
            try {
              await api(`api/tabs/${tab.id}`, {
                method: "PATCH",
                mutation: true,
                body: { name: value },
              });
            } catch (error) {
              window.setTimeout(() => window.alert(error.message), 0);
            }
          }
          renderTabs();
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
        name.className = "tab-name";
        name.textContent = tab.name;
        item.append(name);

        const close = document.createElement("button");
        close.className = "tab-close";
        close.type = "button";
        close.textContent = "×";
        close.title = "Close tab";
        close.setAttribute("aria-label", `Close ${tab.name}`);
        close.addEventListener("click", (event) => {
          event.stopPropagation();
          closeTab(tab);
        });
        item.append(close);

        item.addEventListener("click", () => selectTab(tab.id));
        item.addEventListener("dblclick", (event) => {
          event.preventDefault();
          startRename(tab);
        });
        item.addEventListener("keydown", (event) => {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            selectTab(tab.id);
          }
        });
      }

      els.tabs.append(item);
    }
  }

  function selectTab(id) {
    if (state.activeId === id) return;
    state.activeId = id;
    renderTabs();
    renderActiveTab();
  }

  function startRename(tab) {
    state.editingTabId = tab.id;
    renderTabs();
  }

  async function closeTab(tab) {
    if (!window.confirm(`Close “${tab.name}”? All processes in this tab will be terminated.`)) {
      return;
    }
    try {
      await api(`api/tabs/${tab.id}`, { method: "DELETE", mutation: true });
    } catch (error) {
      if (error.message !== "unauthorized") window.alert(error.message);
    }
    scheduleRefresh(0);
  }

  function renderActiveTab() {
    const tab = state.tabs.find((entry) => entry.id === state.activeId) || null;
    const tabKey = tab ? String(tab.id) : null;

    if (tabKey !== state.renderedTabKey) {
      disposeHandles();
      state.renderedTabKey = tabKey;
    }
    if (!tab) {
      els.panes.replaceChildren();
      return;
    }

    const paneIds = new Set(tab.panes.map((pane) => String(pane.id)));
    for (const [id, handle] of state.handles) {
      if (!paneIds.has(id)) {
        handle.dispose();
        state.handles.delete(id);
      }
    }

    const hasBottom = tab.panes.some((pane) => pane.slot.startsWith("bottom"));
    const hasRight = tab.panes.some((pane) => pane.slot.endsWith("right"));
    els.panes.style.gridTemplateColumns = `repeat(${hasRight ? 2 : 1}, minmax(0, 1fr))`;
    els.panes.style.gridTemplateRows = `repeat(${hasBottom ? 2 : 1}, minmax(0, 1fr))`;

    for (const pane of tab.panes) {
      let handle = state.handles.get(String(pane.id));
      if (!handle) {
        handle = createPane(pane, tab);
        state.handles.set(String(pane.id), handle);
        els.panes.append(handle.el);
        handle.mount();
      }
      handle.update(pane, tab);
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

  function createPane(pane, tab) {
    const el = document.createElement("section");
    el.className = "pane";

    const header = document.createElement("div");
    header.className = "pane-header";
    const dot = document.createElement("span");
    dot.className = "dot connecting";
    dot.title = "connecting";
    const title = document.createElement("span");
    title.className = "pane-title";
    header.append(dot, title);
    el.append(header);

    const host = document.createElement("div");
    host.className = "pane-term";
    el.append(host);

    const term = new window.Terminal({
      cursorBlink: true,
      fontFamily: TERM_FONT,
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 2000,
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
      connected: false,
      reconnecting: false,
      lastSentSize: "",
      fitRaf: 0,
      pane,
      tab,
      renderHeader: () => {},
      update: () => {},
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

    term.onData((data) => sendFrame(CLIENT_INPUT, encoder.encode(data)));
    term.onBinary((data) => {
      const bytes = new Uint8Array(data.length);
      for (let i = 0; i < data.length; i += 1) bytes[i] = data.charCodeAt(i) & 0xff;
      sendFrame(CLIENT_INPUT, bytes);
    });
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown" || !event.ctrlKey || !event.shiftKey) return true;
      if (event.code === "KeyC" && term.hasSelection()) {
        if (navigator.clipboard) navigator.clipboard.writeText(term.getSelection()).catch(() => {});
        return false;
      }
      if (event.code === "KeyV") {
        if (navigator.clipboard) {
          navigator.clipboard
            .readText()
            .then((text) => {
              if (text) term.paste(text);
            })
            .catch(() => {});
        }
        return false;
      }
      return true;
    });

    handle.renderHeader = () => {
      const exited = handle.pane.status && handle.pane.status.state === "exited";
      let cls = "connecting";
      let label = "connecting";
      if (exited) {
        cls = "exited";
        const status = handle.pane.status;
        if (status.signal) label = `exited (${status.signal})`;
        else if (status.code !== null && status.code !== undefined) label = `exited (${status.code})`;
        else label = "exited";
      } else if (handle.connected) {
        cls = "connected";
        label = "connected";
      } else if (handle.reconnecting) {
        cls = "reconnecting";
        label = "reconnecting";
      }
      dot.className = `dot ${cls}`;
      dot.title = label;
      title.textContent = handle.pane.name;
      header.title = `${handle.pane.command}\n${handle.tab.cwd}`;
    };

    handle.update = (nextPane, nextTab) => {
      handle.pane = nextPane;
      handle.tab = nextTab;
      handle.renderHeader();
    };

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
      handle.reconnecting = handle.retry > 0;
      handle.renderHeader();
      const base = new URL("./", window.location.href);
      const wsUrl = new URL(`ws/panes/${handle.pane.id}`, base);
      wsUrl.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(wsUrl);
      ws.binaryType = "arraybuffer";
      handle.ws = ws;

      ws.onopen = () => {
        if (handle.disposed) {
          ws.close();
          return;
        }
        handle.connected = true;
        handle.reconnecting = false;
        handle.retry = 0;
        handle.lastSentSize = "";
        handle.renderHeader();
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
        } else if (opcode === SERVER_DIFF) {
          term.write(new Uint8Array(data, 1));
        }
      };

      ws.onclose = () => {
        handle.connected = false;
        handle.ws = null;
        if (handle.disposed) return;
        handle.reconnecting = true;
        handle.renderHeader();
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
    header.addEventListener("mousedown", () => term.focus());

    return handle;
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
    els.dialogCreate.disabled = true;
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
      if (!els.name.value) els.name.placeholder = basename(listing.path) || "optional";
      els.dialogError.hidden = true;
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
        els.dialogCreate.disabled = false;
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
    els.dialogError.textContent = message;
    els.dialogError.hidden = false;
  }

  async function openNewTab() {
    if (els.dialog.open) return;
    state.pickerRequest += 1;
    els.name.value = "";
    els.name.placeholder = "";
    els.dirPath.value = "";
    els.dirList.replaceChildren();
    els.dirCurrent.textContent = "";
    els.dialogError.hidden = true;
    state.pickerLoading = true;
    els.dialogCreate.disabled = true;
    els.dialog.showModal();
    const remembered = window.localStorage.getItem("mmux.lastDir");
    const target = remembered || state.root;
    const ok = await loadDir(target);
    if (!ok && target !== state.root) await loadDir(state.root);
    els.name.focus();
  }

  els.newTab.addEventListener("click", openNewTab);
  els.emptyNewTab.addEventListener("click", openNewTab);
  els.dialogCancel.addEventListener("click", () => els.dialog.close());

  els.dirPath.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      const value = els.dirPath.value.trim();
      if (value) loadDir(value);
    }
  });

  els.form.addEventListener("submit", async (event) => {
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
    els.dialogCreate.disabled = true;
    try {
      const name = els.name.value.trim();
      const tab = await api("api/tabs", {
        method: "POST",
        mutation: true,
        body: { name: name || null, cwd },
      });
      window.localStorage.setItem("mmux.lastDir", tab.cwd);
      els.dialog.close();
      state.activeId = tab.id;
      await refresh();
    } catch (error) {
      showDialogError(error.message);
    } finally {
      els.dialogCreate.disabled = false;
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

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && state.events) scheduleRefresh(0);
  });

  darkQuery.addEventListener("change", () => {
    const theme = termTheme();
    for (const handle of state.handles.values()) handle.term.options.theme = theme;
  });

  refresh();
})();
