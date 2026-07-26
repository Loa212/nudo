// The browser side of the terminal.
//
// The page is served with a session id and a single-use token. The token is sent
// as the first websocket frame and never appears in the URL, so it stays out of
// browser history and any access log. The client never names a host, a port or a
// command line: the server already knows which target the grant is for.
//
// Frames are length-agnostic JSON in both directions, with an explicit `type`,
// rather than sniffing raw bytes for sentinel strings — output that happened to
// equal a control word would otherwise be misread as one.

(function () {
  "use strict";

  const mount = document.getElementById("terminal");
  if (!mount || typeof Terminal === "undefined") {
    return;
  }

  const config = window.nudoTerminal || {};
  const statusLine = document.getElementById("term-status");

  // How many pending writes to xterm before we ask the server to pause the PTY.
  // Without this, a process spewing output faster than the browser can paint
  // grows the queue without bound.
  const MAX_PENDING_WRITES = 5;
  const RESUME_BELOW = 2;
  const RESIZE_DEBOUNCE_MS = 60;

  let socket = null;
  let pendingWrites = 0;
  let paused = false;
  let closedCleanly = false;
  let resizeTimer = null;

  const term = new Terminal({
    cursorBlink: true,
    convertEol: true,
    fontFamily:
      'ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace',
    fontSize: 13,
    scrollback: 5000,
    theme: { background: "#000000", foreground: "#d6dbe3" },
  });

  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(mount);
  fitNow();
  term.focus();

  function setStatus(text, kind) {
    if (!statusLine) return;
    statusLine.textContent = text;
    statusLine.className = "term-status" + (kind ? " " + kind : "");
  }

  function send(message) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(message));
    }
  }

  function fitNow() {
    try {
      fit.fit();
    } catch (_) {
      // The container can be zero-sized while the page is still laying out;
      // the debounced resize below will retry.
    }
  }

  // Sends the new size only when it actually changed, so a resize gesture does
  // not produce a window-change request per animation frame.
  let lastCols = 0;
  let lastRows = 0;
  function syncSize() {
    fitNow();
    if (term.cols === lastCols && term.rows === lastRows) return;
    lastCols = term.cols;
    lastRows = term.rows;
    send({ type: "resize", cols: term.cols, rows: term.rows });
  }

  function scheduleResize() {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(syncSize, RESIZE_DEBOUNCE_MS);
  }

  window.addEventListener("resize", scheduleResize);
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(scheduleResize).observe(mount);
  }

  function connect() {
    const scheme = window.location.protocol === "https:" ? "wss" : "ws";
    const url =
      scheme +
      "://" +
      window.location.host +
      "/terminal/ws?session=" +
      encodeURIComponent(config.sessionId || "");

    setStatus("connecting…");
    socket = new WebSocket(url);

    socket.onopen = function () {
      // The token is spent here, in the body of the first frame.
      send({ type: "attach", token: config.token || "" });
      setStatus("connected", "");
      syncSize();
      term.focus();
    };

    socket.onmessage = function (event) {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch (_) {
        return;
      }

      if (message.type === "output") {
        pendingWrites += 1;
        term.write(message.data || "", function () {
          pendingWrites = Math.max(0, pendingWrites - 1);
          if (paused && pendingWrites <= RESUME_BELOW) {
            paused = false;
            send({ type: "resume" });
          }
        });

        if (!paused && pendingWrites > MAX_PENDING_WRITES) {
          paused = true;
          send({ type: "pause" });
        }
        return;
      }

      if (message.type === "exit") {
        closedCleanly = true;
        term.write(
          "\r\n\x1b[90m— session ended (exit " + (message.code || 0) + ") —\x1b[0m\r\n",
        );
        setStatus("session ended", "muted");
        return;
      }

      if (message.type === "error") {
        closedCleanly = true;
        term.write("\r\n\x1b[31m" + (message.message || "error") + "\x1b[0m\r\n");
        setStatus(message.message || "error", "bad");
      }
    };

    socket.onclose = function () {
      if (closedCleanly) return;
      // The grant is single-use, so a dropped connection cannot be resumed —
      // reconnecting would fail authentication. Say so rather than retrying.
      term.write("\r\n\x1b[33m— disconnected —\x1b[0m\r\n");
      setStatus("disconnected — open a new session to reconnect", "bad");
    };

    socket.onerror = function () {
      setStatus("connection error", "bad");
    };
  }

  term.onData(function (data) {
    send({ type: "stdin", data: data });
  });

  // Ctrl+C copies when there is a selection, and otherwise reaches the shell as
  // an interrupt — which is what it has to do in a terminal.
  term.attachCustomKeyEventHandler(function (event) {
    if (event.type !== "keydown") return true;
    if ((event.ctrlKey || event.metaKey) && event.key === "c") {
      const selection = term.getSelection();
      if (selection) {
        if (navigator.clipboard) navigator.clipboard.writeText(selection);
        return false;
      }
    }
    if ((event.ctrlKey || event.metaKey) && event.key === "v") {
      // Let the browser paste; xterm receives it through onData.
      return false;
    }
    return true;
  });

  window.addEventListener("beforeunload", function () {
    closedCleanly = true;
    if (socket) socket.close(1000, "page closed");
  });

  connect();
})();
