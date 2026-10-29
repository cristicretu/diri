// diri terminal view: a real terminal in the browser on the gateway's grid
// stream. It paints the same styled rows the Mac paints (`snapshot`, `diff`
// and `resume` frames from /api/session/:id/stream), types through the
// exactly-once /input path, pages history from /history as you scroll up,
// and after a network blip reconnects asking for only the rows that changed.
//
// No framework and no build step. Everything is reachable as
// `window.DiriTerminal`; `DiriTerminal.encode` holds the pure helpers.
"use strict";

(function (global) {
  // ------------------------------------------------------------ pure helpers

  // FNV-1a over the UTF-8 of JSON.stringify(line). The gateway hashes the
  // same bytes (serde_json writes what JSON.stringify reproduces for these
  // values), so a reconnect names each row it still shows in 8 hex digits.
  const encoder = new TextEncoder();
  function digest(line) {
    let hash = 0x811c9dc5;
    for (const byte of encoder.encode(JSON.stringify(line))) {
      hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
    }
    return hash >>> 0;
  }

  function resumeToken(cols, digests) {
    return cols + "x" + digests.length + digests.map((d) => "." + d.toString(16)).join("");
  }

  const ARROWS = { ArrowUp: "A", ArrowDown: "B", ArrowRight: "C", ArrowLeft: "D" };

  function arrow(name, appCursor) {
    return (appCursor ? "\x1bO" : "\x1b[") + ARROWS[name];
  }

  // Ctrl+<ch> as a C0 control, or null if the key has none.
  function control(ch) {
    if (ch.length !== 1) return null;
    const code = ch.toUpperCase().charCodeAt(0);
    if (code >= 64 && code <= 95) return String.fromCharCode(code - 64); // @ A-Z [ \ ] ^ _
    if (ch === " " || ch === "2") return "\x00";
    if (ch === "?" || ch === "8") return "\x7f";
    if (ch >= "3" && ch <= "7") return String.fromCharCode(ch.charCodeAt(0) - 24); // ESC..US
    return null;
  }

  // Bytes for a keydown the textarea should not handle itself, else null.
  function keyBytes(event, modes) {
    const app = !!(modes && modes.appCursor);
    const key = event.key;
    if (event.metaKey) return null; // the browser's copy/paste/select-all
    if (event.ctrlKey && !event.altKey && key.length === 1) return control(key);
    if (event.altKey && !event.ctrlKey && key.length === 1) return "\x1b" + key;
    switch (key) {
      case "Enter": return event.altKey ? "\x1b\r" : "\r";
      case "Backspace": return event.altKey ? "\x1b\x7f" : "\x7f";
      case "Tab": return event.shiftKey ? "\x1b[Z" : "\t";
      case "Escape": return "\x1b";
      case "ArrowUp": case "ArrowDown": case "ArrowLeft": case "ArrowRight":
        return arrow(key, app);
      case "Home": return app ? "\x1bOH" : "\x1b[H";
      case "End": return app ? "\x1bOF" : "\x1b[F";
      case "PageUp": return "\x1b[5~";
      case "PageDown": return "\x1b[6~";
      case "Delete": return "\x1b[3~";
      case "Insert": return "\x1b[2~";
      case "F1": return "\x1bOP";
      case "F2": return "\x1bOQ";
      case "F3": return "\x1bOR";
      case "F4": return "\x1bOS";
      default: return null;
    }
  }

  function base64(text) {
    let binary = "";
    for (const byte of encoder.encode(text)) binary += String.fromCharCode(byte);
    return btoa(binary);
  }

  // xterm's 256-colour cube and grey ramp above the 16 themed colours.
  function indexed(index, palette) {
    if (index < 16) return palette.ansi[index];
    if (index >= 232) {
      const v = 8 + (index - 232) * 10;
      return "rgb(" + v + "," + v + "," + v + ")";
    }
    const n = index - 16;
    const level = (c) => (c === 0 ? 0 : 55 + c * 40);
    return "rgb(" + level(Math.floor(n / 36)) + "," + level(Math.floor(n / 6) % 6) + "," + level(n % 6) + ")";
  }

  const A = { bold: 1, dim: 2, italic: 4, underline: 8, inverse: 16, strike: 32, invisible: 64 };

  // ---------------------------------------------------------------- terminal

  const DEFAULT_PALETTE = {
    fg: "#e6e6e6",
    bg: "#121318",
    ansi: ["#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
           "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff"],
  };
  const HISTORY_PAGE = 200;
  const MAX_GAP = 1000;
  const MIN_FONT = 8;
  const MAX_FONT = 15;
  const STALL_MS = 45000; // two missed 20 s heartbeats

  class DiriTerminal {
    constructor(root, options) {
      this.root = root;
      this.session = options.session;
      this.headers = options.headers || (() => ({}));
      this.onExit = options.onExit || (() => {});
      this.onUnauthorized = options.onUnauthorized || (() => {});
      this.onError = options.onError || (() => {});
      this.cols = 0;
      this.lines = [];
      this.digests = [];
      this.cursor = { x: 0, y: 0, visible: false };
      this.modes = { altScreen: false, appCursor: false, bracketedPaste: false, mouse: false };
      this.palette = DEFAULT_PALETTE;
      this.historySize = 0;
      // Loaded history: rows [start, start + lines.length) of the scrollback.
      this.history = { start: 0, count: 0 };
      this.historyBusy = false;
      this.pinned = true;
      this.outbox = "";
      this.inflight = false;
      this.ctrlArmed = false;
      this.closed = false;
      this.exited = false;
      this.connected = false;
      this.backoff = 250;
      this.lastByte = 0;
      this.stats = { snapshots: 0, resumes: 0, diffs: 0, resumeRows: -1, bytes: 0, connects: 0, batches: 0 };
      this.build();
      DiriTerminal.current = this;
      this.connect();
      this.watchdog = setInterval(() => this.checkStall(), 5000);
      this.onOnline = () => this.reconnectNow();
      this.onVisible = () => { if (!document.hidden) this.reconnectNow(); };
      this.onResize = () => this.fit();
      global.addEventListener("online", this.onOnline);
      document.addEventListener("visibilitychange", this.onVisible);
      global.addEventListener("resize", this.onResize);
      if (global.visualViewport) global.visualViewport.addEventListener("resize", this.onResize);
    }

    // ------------------------------------------------------------------ DOM

    build() {
      const make = (tag, cls) => { const n = document.createElement(tag); n.className = cls; return n; };
      this.root.replaceChildren();
      this.root.classList.add("term");
      this.viewport = make("div", "term-viewport");
      this.historyEl = make("div", "term-history");
      this.liveEl = make("div", "term-live");
      this.cursorEl = make("div", "term-cursor");
      this.banner = make("div", "term-banner");
      this.banner.hidden = true;
      this.input = make("textarea", "term-input");
      for (const [name, value] of [["autocapitalize", "off"], ["autocomplete", "off"],
        ["autocorrect", "off"], ["spellcheck", "false"], ["aria-label", "Terminal input"],
        ["enterkeyhint", "send"], ["rows", "1"]]) this.input.setAttribute(name, value);
      this.liveEl.append(this.cursorEl);
      this.viewport.append(this.historyEl, this.liveEl);
      this.root.append(this.viewport, this.input, this.banner);
      this.resetInput();

      this.viewport.addEventListener("scroll", () => this.onScroll(), { passive: true });
      this.viewport.addEventListener("click", () => {
        if (String(global.getSelection && global.getSelection())) return; // selecting, not typing
        this.focus();
      });
      this.input.addEventListener("keydown", (e) => this.onKeyDown(e));
      this.input.addEventListener("beforeinput", (e) => this.onBeforeInput(e));
      this.input.addEventListener("compositionend", (e) => { this.typed(e.data || ""); this.resetInput(); });
      this.input.addEventListener("paste", (e) => this.onPaste(e));
      // Wheel and touch become mouse-wheel reports when a full-screen
      // program asked for the mouse; otherwise they scroll natively.
      this.viewport.addEventListener("wheel", (e) => this.onWheel(e), { passive: false });
      this.viewport.addEventListener("touchstart", (e) => this.onTouchStart(e), { passive: true });
      this.viewport.addEventListener("touchmove", (e) => this.onTouchMove(e), { passive: false });
      this.measure();
    }

    focus() {
      if (this.exited) return;
      this.input.focus({ preventScroll: true });
    }

    // Glyph advance per pixel of font size, measured once.
    measure() {
      const probe = document.createElement("span");
      probe.className = "term-probe";
      probe.textContent = "0".repeat(50);
      this.root.append(probe);
      this.advance = probe.getBoundingClientRect().width / 50 / 100 || 0.6;
      probe.remove();
    }

    // Largest font (up to MAX_FONT) that fits the grid's width; below
    // MIN_FONT the grid keeps its size and scrolls sideways instead.
    fit() {
      if (!this.cols) return;
      const width = this.viewport.clientWidth - 12;
      const ideal = width / (this.cols * this.advance);
      const font = Math.max(MIN_FONT, Math.min(MAX_FONT, Math.floor(ideal * 4) / 4));
      this.font = font;
      this.cell = font * this.advance;
      this.lineHeight = Math.round(font * 1.3);
      this.root.style.setProperty("--term-font", font + "px");
      this.root.style.setProperty("--term-line", this.lineHeight + "px");
      this.liveEl.style.width = this.historyEl.style.width = Math.ceil(this.cols * this.cell) + "px";
      this.placeCursor();
      if (this.pinned) this.scrollToBottom();
    }

    // Columns that fit this screen at a readable size, for "fit width".
    fittingColumns(font) {
      return Math.max(20, Math.floor((this.viewport.clientWidth - 12) / ((font || 12) * this.advance)));
    }

    paintColor(value) {
      if (value === null || value === undefined) return null;
      if (typeof value === "number") return indexed(value, this.palette);
      return value;
    }

    renderLine(row, line) {
      const parts = [];
      for (const run of (line && line.runs) || []) {
        const span = document.createElement("span");
        span.textContent = run.t;
        let fg = this.paintColor(run.fg);
        let bg = this.paintColor(run.bg);
        const a = run.a || 0;
        if (a & A.inverse) {
          const swap = fg || this.palette.fg;
          fg = bg || this.palette.bg;
          bg = swap;
        }
        if (fg) span.style.color = fg;
        if (bg) span.style.backgroundColor = bg;
        if (a & A.bold) span.style.fontWeight = "700";
        if (a & A.dim) span.style.opacity = "0.6";
        if (a & A.italic) span.style.fontStyle = "italic";
        const deco = [];
        if (a & A.underline) deco.push("underline");
        if (a & A.strike) deco.push("line-through");
        if (deco.length) span.style.textDecoration = deco.join(" ");
        if (a & A.invisible) span.style.color = "transparent";
        parts.push(span);
      }
      row.replaceChildren(...parts);
      row.classList.toggle("wrapped", !!(line && line.wrapped));
    }

    makeRow(line) {
      const row = document.createElement("div");
      row.className = "term-row";
      this.renderLine(row, line);
      return row;
    }

    applyPalette(palette) {
      if (!palette) return;
      this.palette = palette;
      this.root.style.setProperty("--term-fg", palette.fg);
      this.root.style.setProperty("--term-bg", palette.bg);
    }

    applyMeta(frame) {
      if (frame.cursor) this.cursor = frame.cursor;
      if (frame.modes) {
        this.modes = frame.modes;
        this.root.classList.toggle("alt-screen", !!frame.modes.altScreen);
      }
      if (typeof frame.historySize === "number") this.setHistorySize(frame.historySize);
      this.placeCursor();
    }

    placeCursor() {
      const { x, y, visible } = this.cursor;
      this.cursorEl.hidden = !visible || this.exited;
      if (!this.cell) return;
      this.cursorEl.style.transform =
        "translate(" + (x * this.cell) + "px," + (y * this.lineHeight) + "px)";
      this.cursorEl.style.width = this.cell + "px";
      // The hidden textarea follows the cursor so IME candidates and the
      // iOS caret appear where typing lands.
      this.input.style.transform = this.cursorEl.style.transform;
    }

    rowEl(y) {
      return this.liveEl.children[y + 1]; // child 0 is the cursor
    }

    setLine(y, line) {
      if (y < 0 || y >= this.lines.length) return;
      this.lines[y] = line;
      this.digests[y] = digest(line);
      this.renderLine(this.rowEl(y), line);
    }

    applySnapshot(frame) {
      this.applyPalette(frame.palette);
      const resized = frame.cols !== this.cols;
      this.cols = frame.cols;
      this.lines = frame.lines.slice();
      this.digests = this.lines.map(digest);
      this.liveEl.replaceChildren(this.cursorEl, ...this.lines.map((line) => this.makeRow(line)));
      if (resized || !this.font) this.fit();
      this.applyMeta(frame);
    }

    applyRows(map) {
      for (const [key, line] of Object.entries(map || {})) this.setLine(Number(key), line);
    }

    // --------------------------------------------------------------- stream

    url() {
      let url = "/api/session/" + encodeURIComponent(this.session) + "/stream";
      if (this.lines.length && this.cols) url += "?resume=" + resumeToken(this.cols, this.digests);
      return url;
    }

    async connect() {
      if (this.closed || this.exited) return;
      clearTimeout(this.retry);
      const controller = new AbortController();
      this.controller = controller;
      this.stats.connects++;
      let received = false;
      try {
        const response = await fetch(this.url(), {
          headers: this.headers(), signal: controller.signal, cache: "no-store", credentials: "same-origin",
        });
        if (response.status === 401) { this.onUnauthorized(); return; }
        if (response.status === 404) { this.end("Session not found"); return; }
        if (!response.ok || !response.body) throw new Error("HTTP " + response.status);
        this.setConnected(true);
        this.lastByte = Date.now();
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          this.lastByte = Date.now();
          this.stats.bytes += value.length;
          buffer += decoder.decode(value, { stream: true });
          let end;
          while ((end = buffer.indexOf("\n\n")) >= 0) {
            const block = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            if (this.dispatch(block)) received = true;
          }
        }
      } catch (error) {
        if (this.closed) return;
      }
      if (controller !== this.controller) return; // superseded by reconnectNow
      this.setConnected(false);
      if (this.closed || this.exited) return;
      if (received) this.backoff = 250;
      const wait = this.backoff * (0.75 + Math.random() * 0.5);
      this.backoff = Math.min(this.backoff * 2, 5000);
      this.retry = setTimeout(() => this.connect(), wait);
    }

    reconnectNow() {
      if (this.closed || this.exited) return;
      if (this.connected && Date.now() - this.lastByte < 25000) return;
      this.backoff = 250;
      if (this.controller) this.controller.abort();
      this.controller = null;
      this.setConnected(false);
      this.connect();
    }

    checkStall() {
      if (this.connected && Date.now() - this.lastByte > STALL_MS) this.reconnectNow();
    }

    setConnected(connected) {
      this.connected = connected;
      this.root.classList.toggle("stale", !connected && !this.exited);
    }

    // Returns whether the block was a terminal frame.
    dispatch(block) {
      let event = "message";
      const data = [];
      for (const line of block.split("\n")) {
        if (line.startsWith("event:")) event = line.slice(6).trim();
        else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      }
      if (!data.length) return false;
      let frame;
      try { frame = JSON.parse(data.join("\n")); } catch { return false; }
      switch (event) {
        case "snapshot":
          this.stats.snapshots++;
          this.applySnapshot(frame);
          break;
        case "resume":
          this.stats.resumes++;
          this.stats.resumeRows = Object.keys(frame.lines || {}).length;
          this.applyPalette(frame.palette);
          this.applyRows(frame.lines);
          this.applyMeta(frame);
          break;
        case "diff":
          this.stats.diffs++;
          this.applyRows(frame.lines);
          this.applyMeta(frame);
          break;
        case "exit":
          this.end(frame.code === null || frame.code === undefined
            ? "Session ended" : "Session ended (exit " + frame.code + ")");
          this.onExit(frame.code);
          return true;
        default:
          return false;
      }
      if (this.pinned) this.scrollToBottom();
      this.fillIfShort();
      return true;
    }

    end(message) {
      this.exited = true;
      this.setConnected(true);
      this.banner.textContent = message;
      this.banner.hidden = false;
      this.cursorEl.hidden = true;
      this.input.blur();
    }

    close() {
      this.closed = true;
      clearTimeout(this.retry);
      clearInterval(this.watchdog);
      if (this.controller) this.controller.abort();
      global.removeEventListener("online", this.onOnline);
      document.removeEventListener("visibilitychange", this.onVisible);
      global.removeEventListener("resize", this.onResize);
      if (global.visualViewport) global.visualViewport.removeEventListener("resize", this.onResize);
    }

    // -------------------------------------------------------------- history

    scrollToBottom() {
      this.viewport.scrollTop = this.viewport.scrollHeight;
    }

    onScroll() {
      const v = this.viewport;
      this.pinned = v.scrollHeight - v.scrollTop - v.clientHeight < 4;
      if (v.scrollTop < v.clientHeight && !this.modes.altScreen) this.loadOlder();
    }

    // History above a grid that does not fill the screen yet: there is no
    // scrolling to ask for it.
    fillIfShort() {
      const v = this.viewport;
      if (!this.modes.altScreen && !this.historyBusy && v.scrollHeight <= v.clientHeight + 1) {
        this.loadOlder();
      }
    }

    clearHistory() {
      this.history = { start: 0, count: 0 };
      this.historyEl.replaceChildren();
    }

    // The scrollback grew (rows scrolled off the top) or shrank (cleared).
    setHistorySize(size) {
      const previous = this.historySize;
      this.historySize = size;
      if (!this.history.count || size === previous) return;
      const end = this.history.start + this.history.count;
      if (size < end || this.pinned || size - end > MAX_GAP) {
        // At the live edge nobody is looking at history: drop it and load
        // again on the way up rather than chase every scrolled-off row.
        this.clearHistory();
        return;
      }
      this.loadRange(size, size - end, "append");
    }

    loadOlder() {
      if (this.historyBusy) return;
      if (!this.history.count) {
        if (this.historySize > 0) this.loadRange(this.historySize, HISTORY_PAGE, "prepend");
        return;
      }
      if (this.history.start > 0) this.loadRange(this.history.start, HISTORY_PAGE, "prepend");
    }

    async loadRange(before, limit, where) {
      if (this.historyBusy) return;
      this.historyBusy = true;
      try {
        const path = "/api/session/" + encodeURIComponent(this.session) +
          "/history?before=" + before + "&limit=" + Math.min(limit, MAX_GAP);
        const response = await fetch(path, { headers: this.headers(), cache: "no-store", credentials: "same-origin" });
        if (!response.ok) return;
        const page = await response.json();
        const rows = (page.lines || []).map((line) => this.makeRow(line));
        if (!rows.length) return;
        if (where === "prepend") {
          const expected = this.history.count ? this.history.start : this.historySize;
          if (page.start + rows.length !== expected) return; // raced a reset
          const v = this.viewport;
          const height = v.scrollHeight;
          const top = v.scrollTop;
          this.historyEl.prepend(...rows);
          if (!this.history.count) this.history.count = 0;
          this.history = { start: page.start, count: this.history.count + rows.length };
          v.scrollTop = top + (v.scrollHeight - height); // keep what is on screen still
        } else {
          if (page.start !== this.history.start + this.history.count) return;
          this.historyEl.append(...rows);
          this.history.count += rows.length;
        }
      } catch {
        // Offline: the next scroll asks again.
      } finally {
        this.historyBusy = false;
      }
      this.fillIfShort();
    }

    // ---------------------------------------------------------------- input

    resetInput() {
      // One space keeps a soft keyboard's backspace firing an input event.
      this.input.value = " ";
      try { this.input.setSelectionRange(1, 1); } catch { /* not focused */ }
    }

    onKeyDown(event) {
      if (event.isComposing || event.keyCode === 229) return;
      let bytes = keyBytes(event, this.modes);
      if (bytes === null && this.ctrlArmed && event.key.length === 1 && !event.metaKey) {
        bytes = control(event.key);
        this.setCtrl(false);
      }
      if (bytes === null) return;
      event.preventDefault();
      this.send(bytes);
    }

    onBeforeInput(event) {
      switch (event.inputType) {
        case "insertText":
        case "insertReplacementText":
          event.preventDefault();
          this.typed(event.data || (event.dataTransfer && event.dataTransfer.getData("text")) || "");
          break;
        case "insertLineBreak":
        case "insertParagraph":
          event.preventDefault();
          this.send("\r");
          break;
        case "deleteContentBackward":
          event.preventDefault();
          this.send("\x7f");
          break;
        case "deleteWordBackward":
          event.preventDefault();
          this.send("\x17");
          break;
        default:
          return; // composition and the rest finish natively; compositionend sends
      }
      this.resetInput();
    }

    typed(text) {
      if (!text) return;
      if (this.ctrlArmed) {
        const code = control(text[0]);
        this.setCtrl(false);
        if (code !== null) {
          this.send(code + text.slice(1));
          return;
        }
      }
      this.send(text);
    }

    onPaste(event) {
      event.preventDefault();
      const text = ((event.clipboardData && event.clipboardData.getData("text")) || "").replace(/\r?\n/g, "\r");
      if (!text) return;
      this.send(this.modes.bracketedPaste ? "\x1b[200~" + text + "\x1b[201~" : text);
    }

    setCtrl(armed) {
      this.ctrlArmed = armed;
      this.root.classList.toggle("ctrl-armed", armed);
      if (this.onCtrl) this.onCtrl(armed);
    }

    // A key-row key by name: esc, tab, backtab, up, down, left, right,
    // enter, ctrl (sticky: applies to the next key), or literal text.
    press(name) {
      const names = {
        esc: "\x1b", tab: "\t", backtab: "\x1b[Z", enter: "\r",
        up: arrow("ArrowUp", this.modes.appCursor), down: arrow("ArrowDown", this.modes.appCursor),
        left: arrow("ArrowLeft", this.modes.appCursor), right: arrow("ArrowRight", this.modes.appCursor),
      };
      if (name === "ctrl") {
        this.setCtrl(!this.ctrlArmed);
        return;
      }
      if (names[name] !== undefined) {
        this.setCtrl(false);
        this.send(names[name]);
      } else {
        this.typed(name);
      }
    }

    // Queues bytes; one batch in flight at a time, so order holds. A batch
    // keeps its id across retries and the gateway's ledger writes it once,
    // so a POST lost to a cell handoff is simply sent again.
    send(bytes) {
      if (!bytes || this.exited) return;
      this.outbox += bytes;
      this.pinned = true;
      this.scrollToBottom();
      this.flush();
    }

    async flush() {
      if (this.inflight || !this.outbox) return;
      this.inflight = true;
      const batch = {
        id: Date.now().toString(36) + Math.random().toString(36).slice(2, 10),
        data: base64(this.outbox),
      };
      this.outbox = "";
      let delay = 250;
      for (;;) {
        let status = 0;
        try {
          const response = await fetch("/api/session/" + encodeURIComponent(this.session) + "/input", {
            method: "POST",
            headers: Object.assign({ "Content-Type": "application/json" }, this.headers()),
            body: JSON.stringify(batch),
            cache: "no-store",
            credentials: "same-origin",
          });
          status = response.status;
        } catch { /* network: retry */ }
        if (status >= 200 && status < 300) break;
        if (status === 401) { this.onUnauthorized(); break; }
        if (status >= 400 && status < 500) { this.onError("Input was refused"); break; }
        if (this.closed || this.exited) { this.inflight = false; return; }
        await new Promise((resolve) => setTimeout(resolve, delay));
        delay = Math.min(delay * 2, 4000);
      }
      this.stats.batches++;
      this.inflight = false;
      if (this.outbox) this.flush();
    }

    // ---------------------------------------------------------------- mouse

    wantsWheel() {
      return this.modes.altScreen && this.modes.mouse;
    }

    // SGR wheel reports (buttons 64/65) at the pointer's cell.
    wheel(up, clientX, clientY, steps) {
      const box = this.liveEl.getBoundingClientRect();
      const col = Math.max(1, Math.min(this.cols, Math.floor((clientX - box.left) / this.cell) + 1));
      const row = Math.max(1, Math.min(this.lines.length, Math.floor((clientY - box.top) / this.lineHeight) + 1));
      this.send(("\x1b[<" + (up ? 64 : 65) + ";" + col + ";" + row + "M").repeat(steps));
    }

    onWheel(event) {
      if (!this.wantsWheel()) return;
      event.preventDefault();
      const steps = Math.max(1, Math.min(10, Math.round(Math.abs(event.deltaY) / (this.lineHeight || 16))));
      this.wheel(event.deltaY < 0, event.clientX, event.clientY, steps);
    }

    onTouchStart(event) {
      this.touchY = event.touches[0].clientY;
    }

    onTouchMove(event) {
      if (!this.wantsWheel() || event.touches.length !== 1) return;
      event.preventDefault();
      const touch = event.touches[0];
      const moved = touch.clientY - this.touchY;
      const steps = Math.trunc(moved / (this.lineHeight || 16));
      if (!steps) return;
      this.touchY += steps * (this.lineHeight || 16);
      // Finger down drags content down: earlier lines, i.e. wheel up.
      this.wheel(steps > 0, touch.clientX, touch.clientY, Math.min(10, Math.abs(steps)));
    }
  }

  DiriTerminal.encode = { digest, resumeToken, keyBytes, control, arrow, base64, indexed };
  global.DiriTerminal = DiriTerminal;
})(typeof window !== "undefined" ? window : globalThis);
