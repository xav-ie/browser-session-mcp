// Takeover client: speaks CDP over a WebSocket to the takeover daemon, which
// relays it to the tab (gated on this link's claim, filtered to the session).
// Renders the screencast, forwards input, manages tabs, and relays vault
// autofill. The daemon also serves this page and the /claim and /done endpoints.

// Token comes from the URL (/takeover/<token>) — no server-side templating.
const TOKEN =
  (location.pathname.match(/\/takeover\/([0-9a-f]{32})/) || [])[1] || "";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const ctx = canvas.getContext("2d") as CanvasRenderingContext2D;
const doneBtn = document.getElementById("done") as HTMLButtonElement;
const tabsEl = document.getElementById("tabs") as HTMLElement;
const wrapEl = document.getElementById("wrap") as HTMLElement;
const overlayEl = document.getElementById("overlay") as HTMLElement;
const urlEl = document.getElementById("url") as HTMLInputElement;
const toastEl = document.getElementById("toast") as HTMLElement;

let nextId = 1;
let lastMeta: any = null; // most recent screencastFrame metadata (for coord mapping)
let ws: WebSocket | null = null;
let wsBase: string | null = null; // this daemon's relay, e.g. wss://<host>/takeover/<token>
let ctxId: string | null = null; // this session's browserContextId — tabs filtered to it
let currentTarget: string | null = null; // targetId currently screencast
let getTargetsId: number | null = null;
let createTargetId: number | null = null;
let urlQueryId: number | null = null;
let navHistoryId: number | null = null; // pending Page.getNavigationHistory id
let refreshTimer: any = null;
let lastTabs: any[] = [];
const tabOrder: string[] = []; // stable display order (Target.getTargets is not ordered)

// Inline SVG icons (stroke inherits currentColor).
const SVG =
  'viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"';
const svg = (size: number, body: string) =>
  '<svg width="' +
  size +
  '" height="' +
  size +
  '" ' +
  SVG +
  ">" +
  body +
  "</svg>";
const ICONS = {
  back: svg(16, '<path d="M15 18l-6-6 6-6"/>'),
  fwd: svg(16, '<path d="M9 18l6-6-6-6"/>'),
  plus: svg(14, '<path d="M12 5v14M5 12h14"/>'),
  close: svg(13, '<path d="M18 6 6 18M6 6l12 12"/>'),
  enter: svg(
    14,
    '<path d="M9 10l-4 4 4 4"/><path d="M5 14h11a4 4 0 0 0 4-4V6"/>',
  ),
  key: svg(
    16,
    '<circle cx="7.5" cy="15.5" r="4.5"/><path d="M10.7 12.3 21 2M16 7l3 3M19 4l2 2"/>',
  ),
  plus16: svg(16, '<path d="M12 5v14M5 12h14"/>'),
};

function send(method: string, params?: any): number | null {
  if (!ws || ws.readyState !== WebSocket.OPEN) return null;
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params: params || {} }));
  return id;
}
// Status goes to a toast at the bottom: progress ("claiming session…") and
// errors stay until the next status, info (relay results) fades after a few
// seconds, and reaching "connected" clears it.
type StatusKind = "pending" | "ok" | "err" | "info";
let toastTimer: any = null;
function setStatus(s: string, kind: StatusKind = "info") {
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = null;
  toastEl.hidden = kind === "ok";
  if (kind === "ok") return;
  toastEl.textContent = s;
  toastEl.dataset.kind = kind;
  if (kind === "info")
    toastTimer = setTimeout(() => (toastEl.hidden = true), 3000);
}

// Every tab renders at the session viewport the agent chose (from the claim),
// never at this window's size: resizing would reflow the page out from under
// the agent and break its coordinates. Re-applying the same size is a no-op for
// tabs already at it and pulls fresh tabs/popups in line. The canvas is scaled
// locally to fit this window instead (fitCanvas).
let sessionVp = { width: 1280, height: 800 };
function applyViewport() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const { width, height } = sessionVp;
  send("Emulation.setDeviceMetricsOverride", {
    width,
    height,
    deviceScaleFactor: 1,
    mobile: false,
  });
  try {
    send("Page.stopScreencast");
  } catch {}
  send("Page.startScreencast", {
    format: "jpeg",
    quality: 70,
    everyNthFrame: 1,
    maxWidth: width,
    maxHeight: height,
  });
}

// --- local view: fit + pinch zoom ------------------------------------------
// The canvas is fit ("contain") into #wrap, then a local transform zooms/pans
// it (two-finger pinch). Purely client-side — the remote page never sees it,
// and toPage() maps through getBoundingClientRect, which includes the transform.
const view = { zoom: 1, x: 0, y: 0 };
const MAX_ZOOM = 5;
function fitCanvas() {
  // Fit inside #wrap's padding, which leaves room for the canvas outline
  // (drawn outside the box, so #wrap's overflow clip would cut it off).
  const cs = getComputedStyle(wrapEl);
  const w =
    wrapEl.clientWidth -
    parseFloat(cs.paddingLeft) -
    parseFloat(cs.paddingRight);
  const h =
    wrapEl.clientHeight -
    parseFloat(cs.paddingTop) -
    parseFloat(cs.paddingBottom);
  if (w <= 0 || h <= 0) return;
  // Scale up as well as down: a big window should get a big view, not a
  // 1280x800 island in the middle.
  const fit = Math.min(w / canvas.width, h / canvas.height);
  canvas.style.width = canvas.width * fit + "px";
  canvas.style.height = canvas.height * fit + "px";
  setView(view.zoom, view.x, view.y);
}
// Keep the zoomed canvas covering #wrap (or centered on an axis where it's
// smaller), so you can't pan the page off into the void.
function clampAxis(t: number, off: number, scaled: number, wrapSize: number) {
  if (scaled <= wrapSize) return (wrapSize - scaled) / 2 - off;
  return Math.min(-off, Math.max(wrapSize - scaled - off, t));
}
function setView(zoom: number, x: number, y: number) {
  view.zoom = Math.min(MAX_ZOOM, Math.max(1, zoom));
  const w = canvas.offsetWidth * view.zoom;
  const h = canvas.offsetHeight * view.zoom;
  view.x = clampAxis(x, canvas.offsetLeft, w, wrapEl.clientWidth);
  view.y = clampAxis(y, canvas.offsetTop, h, wrapEl.clientHeight);
  canvas.style.transform =
    view.zoom === 1
      ? ""
      : `translate(${view.x}px, ${view.y}px) scale(${view.zoom})`;
}

// --- vault relay ----------------------------------------------------------
function relay(id: string) {
  const el = document.getElementById(id) as HTMLInputElement | null;
  const text = (el && el.value) || "";
  if (!text) {
    setStatus("nothing to send: autofill the box first, then try again");
    return;
  }
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    setStatus("not connected", "err");
    return;
  }
  const a = {
    key: "a",
    code: "KeyA",
    windowsVirtualKeyCode: 65,
    nativeVirtualKeyCode: 65,
    modifiers: 2,
  };
  send("Input.dispatchKeyEvent", { type: "keyDown", ...a });
  send("Input.dispatchKeyEvent", { type: "keyUp", ...a });
  send("Input.insertText", { text });
  setStatus(
    (id === "vp" ? "password" : "username") + " sent to the focused field",
  );
}

// --- tabs -----------------------------------------------------------------
const tabWsUrl = (targetId: string) => wsBase + "/devtools/page/" + targetId;

function openTab(targetId: string) {
  if (ws) {
    try {
      send("Page.stopScreencast");
    } catch {}
    try {
      ws.close();
    } catch {}
    ws = null;
  }
  currentTarget = targetId;
  renderTabs();
  connect(tabWsUrl(targetId));
}

function refreshTabs() {
  if (ws && ws.readyState === WebSocket.OPEN)
    getTargetsId = send("Target.getTargets");
}
function scheduleRefresh() {
  if (refreshTimer) return;
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    refreshTabs();
  }, 300);
}

function closeTab(targetId: string) {
  const others = tabOrder.filter((id) => id !== targetId);
  send("Target.closeTarget", { targetId }); // over the CURRENT (open) ws first
  if (targetId === currentTarget && others.length) {
    openTab(others[0]);
  } else {
    scheduleRefresh();
  }
}

function renderTabs(infos?: any[]) {
  if (infos) {
    lastTabs = infos.filter(
      (t) => t.type === "page" && t.browserContextId === ctxId,
    );
  }
  // Stable order: drop destroyed, append newly-seen — so back/forward (which
  // reshuffles Target.getTargets) doesn't reorder the bar.
  const byId = new Map(lastTabs.map((t) => [t.targetId, t]));
  for (let i = tabOrder.length - 1; i >= 0; i--) {
    if (!byId.has(tabOrder[i])) tabOrder.splice(i, 1);
  }
  for (const t of lastTabs) {
    if (!tabOrder.includes(t.targetId)) tabOrder.push(t.targetId);
  }

  tabsEl.innerHTML = "";
  const multi = tabOrder.length > 1;
  for (const id of tabOrder) {
    const t = byId.get(id);
    const b = document.createElement("button");
    b.className = "tab" + (id === currentTarget ? " active" : "");
    b.title = t.url || "";
    const label = document.createElement("span");
    label.className = "tablabel";
    label.textContent = (t.title && t.title.trim()) || t.url || "(untitled)";
    b.appendChild(label);
    if (multi) {
      const x = document.createElement("span");
      x.className = "tabclose";
      x.innerHTML = ICONS.close;
      x.title = "Close tab";
      x.addEventListener("click", (e) => {
        e.stopPropagation();
        closeTab(id);
      });
      b.appendChild(x);
    }
    b.addEventListener("click", () => {
      if (id !== currentTarget) openTab(id);
    });
    tabsEl.appendChild(b);
  }
  // A single tab needs no strip; the bar's + button opens more.
  tabsEl.hidden = !multi;
}

// Replies awaited by call() (one-off requests that need their result).
const pending = new Map<number, (result: any) => void>();
function call(method: string, params?: any): Promise<any> {
  const id = send(method, params);
  if (id === null) return Promise.reject(new Error("not connected"));
  return new Promise((resolve) => pending.set(id, resolve));
}

function onMessage(ev: MessageEvent) {
  let msg: any;
  try {
    msg = JSON.parse(ev.data);
  } catch {
    return;
  }

  if (msg.id && pending.has(msg.id)) {
    const resolve = pending.get(msg.id)!;
    pending.delete(msg.id);
    resolve(msg.result);
    return;
  }

  if (msg.method === "Page.screencastFrame") {
    const { data, sessionId, metadata } = msg.params;
    lastMeta = metadata;
    const img = new Image();
    img.onload = () => {
      if (canvas.width !== img.width || canvas.height !== img.height) {
        canvas.width = img.width;
        canvas.height = img.height;
        fitCanvas();
      }
      ctx.drawImage(img, 0, 0);
    };
    img.src = "data:image/jpeg;base64," + data;
    send("Page.screencastFrameAck", { sessionId }); // keep frames flowing
    return;
  }
  if (
    msg.id &&
    msg.id === getTargetsId &&
    msg.result &&
    msg.result.targetInfos
  ) {
    renderTabs(msg.result.targetInfos);
    return;
  }
  if (
    msg.id &&
    msg.id === createTargetId &&
    msg.result &&
    msg.result.targetId
  ) {
    openTab(msg.result.targetId); // jump to the freshly-opened tab
    return;
  }
  if (msg.id && msg.id === urlQueryId && msg.result && msg.result.result) {
    if (document.activeElement !== urlEl)
      urlEl.value = msg.result.result.value || "";
    return;
  }
  if (msg.id && msg.id === navHistoryId && msg.result) {
    // currentIndex points at the active entry; back/forward are possible iff
    // there's an entry on the respective side of it.
    const idx = msg.result.currentIndex;
    const count = (msg.result.entries || []).length;
    backBtn.disabled = idx <= 0;
    fwdBtn.disabled = idx >= count - 1;
    return;
  }
  if (
    msg.method === "Page.frameNavigated" &&
    msg.params.frame &&
    !msg.params.frame.parentId
  ) {
    if (document.activeElement !== urlEl)
      urlEl.value = msg.params.frame.url || "";
    refreshNav();
    return;
  }
  if (
    msg.method === "Target.targetCreated" ||
    msg.method === "Target.targetInfoChanged" ||
    msg.method === "Target.targetDestroyed"
  ) {
    scheduleRefresh();
  }
}

function connect(wsUrl: string) {
  ws = new WebSocket(wsUrl);
  ws.onopen = () => {
    setStatus("connected", "ok");
    send("Page.enable");
    // NOTE: deliberately NOT Runtime.enable — it turns on console-event
    // reporting, which is the CDP tell bot-detectors catch. Runtime.evaluate
    // (used for the URL/nav) works without it.
    send("Target.setDiscoverTargets", { discover: true });
    send("Page.bringToFront");
    if (currentTarget) {
      try {
        send("Target.activateTarget", { targetId: currentTarget });
      } catch {}
    }
    applyViewport();
    urlQueryId = send("Runtime.evaluate", {
      expression: "location.href",
      returnByValue: true,
    });
    doneBtn.disabled = false;
    refreshNav();
    canvas.focus();
    refreshTabs();
  };
  const sock = ws;
  // Switching tabs or pressing Done drops `ws` first, so only an unexpected
  // close (Chrome restarted, network gone) still finds this socket current.
  ws.onclose = () => {
    if (ws === sock) setStatus("connection lost, reload to reconnect", "err");
  };
  ws.onerror = () =>
    setStatus("connection error: is chrome.<base> reachable?", "err");
  ws.onmessage = onMessage;
}

// --- input forwarding -----------------------------------------------------
// Displayed canvas (fit + local zoom) → the page's viewport in CSS pixels. The
// frame shows the whole device screen, so map by the frame's device size, like
// DevTools' own screencast does; pageScaleFactor is already baked into the
// image, so dividing by it again misaims every click once it isn't 1.
function toPage(ev: { clientX: number; clientY: number }) {
  const rect = canvas.getBoundingClientRect();
  const w = (lastMeta && lastMeta.deviceWidth) || canvas.width;
  const h = (lastMeta && lastMeta.deviceHeight) || canvas.height;
  const offTop = (lastMeta && lastMeta.offsetTop) || 0;
  return {
    x: ((ev.clientX - rect.left) * w) / rect.width,
    y: ((ev.clientY - rect.top) * h) / rect.height - offTop,
  };
}
// Remote CSS pixels per displayed CSS pixel (for turning drags into scrolls).
function pageUnitsPerPx() {
  const w = (lastMeta && lastMeta.deviceWidth) || canvas.width;
  return w / canvas.getBoundingClientRect().width;
}
const BUTTONS: Record<number, string> = { 0: "left", 1: "middle", 2: "right" };
function mouse(type: string, ev: MouseEvent) {
  const { x, y } = toPage(ev);
  send("Input.dispatchMouseEvent", {
    type,
    x,
    y,
    button: BUTTONS[ev.button] || "none",
    buttons: ev.buttons,
    clickCount:
      type === "mousePressed" || type === "mouseReleased" ? ev.detail || 1 : 0,
    modifiers: modBits(ev),
  });
}
function modBits(ev: MouseEvent | KeyboardEvent | WheelEvent) {
  return (
    (ev.altKey ? 1 : 0) |
    (ev.ctrlKey ? 2 : 0) |
    (ev.metaKey ? 4 : 0) |
    (ev.shiftKey ? 8 : 0)
  );
}
canvas.addEventListener("mousemove", (e) => mouse("mouseMoved", e));
canvas.addEventListener("mousedown", (e) => {
  canvas.focus();
  mouse("mousePressed", e);
});
canvas.addEventListener("mouseup", (e) => mouse("mouseReleased", e));
canvas.addEventListener("contextmenu", (e) => e.preventDefault());
canvas.addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    const { x, y } = toPage(e);
    // Pass the DOM wheel delta through unchanged — CDP's mouseWheel uses the same
    // sign convention, so negating it scrolled backwards.
    send("Input.dispatchMouseEvent", {
      type: "mouseWheel",
      x,
      y,
      deltaX: e.deltaX,
      deltaY: e.deltaY,
      modifiers: modBits(e),
    });
  },
  { passive: false },
);

// --- touch ----------------------------------------------------------------
// Phones send no real mouse: one finger taps (→ click) or drags (→ scrolls the
// remote page); two fingers pinch-zoom/pan the local view so the full-size
// viewport is readable on a small screen. `touch-action: none` on the canvas
// keeps the browser from zooming the takeover page itself, and cancelling
// pointerdown suppresses the compatibility mouse events (no double input).
const TAP_SLOP = 10;
const touches = new Map<number, { x: number; y: number }>();
let gesture:
  | { kind: "tap" | "scroll"; sx: number; sy: number; lx: number; ly: number }
  | {
      kind: "pinch";
      dist: number;
      cx: number;
      cy: number;
      zoom: number;
      x: number;
      y: number;
    }
  | { kind: "none" }
  | null = null;
function pinchState() {
  const [a, b] = [...touches.values()];
  return {
    dist: Math.hypot(a.x - b.x, a.y - b.y) || 1,
    cx: (a.x + b.x) / 2,
    cy: (a.y + b.y) / 2,
  };
}
function tap(ev: PointerEvent) {
  const { x, y } = toPage(ev);
  const base = { x, y, button: "left", modifiers: 0 };
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mouseMoved",
    button: "none",
  });
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mousePressed",
    buttons: 1,
    clickCount: 1,
  });
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mouseReleased",
    buttons: 0,
    clickCount: 1,
  });
}
canvas.addEventListener("pointerdown", (e) => {
  if (e.pointerType === "mouse") return;
  e.preventDefault();
  canvas.focus();
  canvas.setPointerCapture(e.pointerId);
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (touches.size === 1) {
    gesture = {
      kind: "tap",
      sx: e.clientX,
      sy: e.clientY,
      lx: e.clientX,
      ly: e.clientY,
    };
  } else if (touches.size === 2) {
    gesture = {
      kind: "pinch",
      ...pinchState(),
      zoom: view.zoom,
      x: view.x,
      y: view.y,
    };
  } else {
    gesture = { kind: "none" };
  }
});
canvas.addEventListener("pointermove", (e) => {
  if (e.pointerType === "mouse" || !touches.has(e.pointerId) || !gesture)
    return;
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (gesture.kind === "tap" || gesture.kind === "scroll") {
    if (
      gesture.kind === "tap" &&
      Math.hypot(e.clientX - gesture.sx, e.clientY - gesture.sy) > TAP_SLOP
    ) {
      gesture.kind = "scroll";
    }
    if (gesture.kind === "scroll") {
      const k = pageUnitsPerPx();
      const { x, y } = toPage(e);
      send("Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x,
        y,
        deltaX: (gesture.lx - e.clientX) * k,
        deltaY: (gesture.ly - e.clientY) * k,
      });
    }
    gesture.lx = e.clientX;
    gesture.ly = e.clientY;
  } else if (gesture.kind === "pinch") {
    // Zoom about the fingers' midpoint: the content point that was under the
    // starting midpoint stays under the current one.
    const p = pinchState();
    const zoom = Math.min(
      MAX_ZOOM,
      Math.max(1, (gesture.zoom * p.dist) / gesture.dist),
    );
    const r = wrapEl.getBoundingClientRect();
    const ox = r.left + canvas.offsetLeft;
    const oy = r.top + canvas.offsetTop;
    const ux = (gesture.cx - ox - gesture.x) / gesture.zoom;
    const uy = (gesture.cy - oy - gesture.y) / gesture.zoom;
    setView(zoom, p.cx - ox - ux * zoom, p.cy - oy - uy * zoom);
  }
});
function touchEnd(e: PointerEvent) {
  if (e.pointerType === "mouse" || !touches.has(e.pointerId)) return;
  touches.delete(e.pointerId);
  if (e.type === "pointerup" && gesture && gesture.kind === "tap") tap(e);
  // Lifting one finger of a pinch must not turn into a stray tap or scroll.
  gesture = touches.size ? { kind: "none" } : null;
}
canvas.addEventListener("pointerup", touchEnd);
canvas.addEventListener("pointercancel", touchEnd);

function keyEvent(type: string, ev: KeyboardEvent) {
  const isChar = type === "keyDown" && ev.key.length === 1;
  send("Input.dispatchKeyEvent", {
    type: isChar ? "keyDown" : type,
    key: ev.key,
    code: ev.code,
    windowsVirtualKeyCode: ev.keyCode,
    nativeVirtualKeyCode: ev.keyCode,
    text: ev.key.length === 1 ? ev.key : undefined,
    modifiers: modBits(ev),
  });
}
// ⌘V / Ctrl+V pastes THIS machine's clipboard: forwarded as keys it would
// paste the remote browser's (empty) clipboard, or type a stray "v" since Linux
// Chrome ignores Cmd. So the shortcut stays local, the browser fires `paste`,
// and the text is typed into the remote page's focused field.
const isPasteKey = (e: KeyboardEvent) =>
  (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "v";
let pasteFallback: any = null;
function pasteText(text: string) {
  if (!text) return;
  send("Input.insertText", { text });
  setStatus("pasted " + text.length + " characters");
}
// ⌘C / Ctrl+C copies the remote page's selection to THIS machine's clipboard
// (text selected in an input/textarea too; never a password field's).
const isCopyKey = (e: KeyboardEvent) =>
  (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "c";
const SELECTION_JS = `(() => {
  const a = document.activeElement;
  if (a && (a.tagName === "TEXTAREA" || (a.tagName === "INPUT" && a.type !== "password"))
      && typeof a.selectionStart === "number")
    return a.value.slice(a.selectionStart, a.selectionEnd);
  return String(getSelection());
})()`;
function copySelection() {
  const text = call("Runtime.evaluate", {
    expression: SELECTION_JS,
    returnByValue: true,
  }).then((r) => String((r && r.result && r.result.value) || ""));
  // A promise-valued ClipboardItem is written inside this key press, which
  // Safari requires; the remote answer arrives a moment later.
  const write =
    typeof ClipboardItem !== "undefined"
      ? navigator.clipboard.write([
          new ClipboardItem({
            "text/plain": text.then((t) => {
              if (!t) throw new Error("empty");
              return new Blob([t], { type: "text/plain" });
            }),
          }),
        ])
      : text.then((t) => {
          if (!t) throw new Error("empty");
          return navigator.clipboard.writeText(t);
        });
  Promise.all([text, write])
    .then(([t]) => setStatus("copied " + t.length + " characters"))
    .catch(() =>
      text.then(
        (t) =>
          setStatus(
            t ? "couldn't write the clipboard" : "nothing selected",
            t ? "err" : "info",
          ),
        () => setStatus("not connected", "err"),
      ),
    );
}

canvas.addEventListener("keydown", (e) => {
  if (isCopyKey(e)) {
    e.preventDefault();
    copySelection();
    return;
  }
  if (isPasteKey(e)) {
    // Safari fires no `paste` on a non-editable element; read the clipboard
    // directly there (it shows its own Paste prompt).
    pasteFallback = setTimeout(() => {
      navigator.clipboard
        ?.readText()
        .then(pasteText)
        .catch(() => setStatus("clipboard not readable here", "err"));
    }, 50);
    return;
  }
  e.preventDefault();
  keyEvent("keyDown", e);
});
canvas.addEventListener("keyup", (e) => {
  if (isPasteKey(e) || isCopyKey(e)) return;
  e.preventDefault();
  keyEvent("keyUp", e);
});
document.addEventListener("paste", (e) => {
  if (document.activeElement !== canvas) return; // URL bar, vault: normal paste
  e.preventDefault();
  clearTimeout(pasteFallback);
  pasteText(e.clipboardData?.getData("text/plain") || "");
});

// Re-fit the local canvas whenever the screencast area changes size: window
// resizes, and the tab strip appearing or disappearing. The remote viewport
// stays at the session size.
new ResizeObserver(fitCanvas).observe(wrapEl);

// --- address bar + back/forward ------------------------------------------
function navTo() {
  let u = urlEl.value.trim();
  if (!u) return;
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(u)) u = "https://" + u;
  send("Page.navigate", { url: u });
  urlEl.blur();
}
const backBtn = document.getElementById("back") as HTMLButtonElement;
const fwdBtn = document.getElementById("fwd") as HTMLButtonElement;
backBtn.innerHTML = ICONS.back;
fwdBtn.innerHTML = ICONS.fwd;
(document.getElementById("keys") as HTMLElement).innerHTML = ICONS.key;
const newTabBtn = document.getElementById("newtab") as HTMLElement;
newTabBtn.innerHTML = ICONS.plus16;
newTabBtn.addEventListener("click", () => {
  createTargetId = send("Target.createTarget", {
    url: "about:blank",
    browserContextId: ctxId,
  });
});
// Ask Chrome for the navigation history so we can enable/disable back & forward.
// Re-run after every navigation (frameNavigated) and after clicking the buttons,
// since same-document history moves (e.g. hash changes) don't fire frameNavigated.
function refreshNav() {
  navHistoryId = send("Page.getNavigationHistory");
}
backBtn.addEventListener("click", () => {
  if (backBtn.disabled) return;
  send("Runtime.evaluate", { expression: "history.back()" });
  setTimeout(refreshNav, 150);
});
fwdBtn.addEventListener("click", () => {
  if (fwdBtn.disabled) return;
  send("Runtime.evaluate", { expression: "history.forward()" });
  setTimeout(refreshNav, 150);
});

urlEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    navTo();
  }
});

// --- done -----------------------------------------------------------------
doneBtn.addEventListener("click", async () => {
  doneBtn.disabled = true;
  setStatus("handing back to agent…", "pending");
  try {
    const r = await fetch("/takeover/" + TOKEN + "/done", { method: "POST" });
    if (!r.ok) throw new Error("done failed: HTTP " + r.status);
    if (ws) {
      try {
        send("Page.stopScreencast");
      } catch {}
      const sock = ws;
      ws = null; // an intentional close, not "connection lost"
      sock.close();
    }
    setStatus("done, you can close this tab", "ok");
    document.querySelectorAll<HTMLElement>("[popover]").forEach((p) => {
      try {
        p.hidePopover();
      } catch {}
    });
    overlayEl.textContent =
      "Handed back to the agent — this session is no longer interactive. You can close this tab.";
    overlayEl.style.display = "flex";
  } catch {
    setStatus("failed to signal done, try again", "err");
    doneBtn.disabled = false;
  }
});

document.querySelectorAll("#vault button[data-for]").forEach((b) => {
  b.insertAdjacentHTML("afterbegin", ICONS.enter);
  b.addEventListener("click", (e) => {
    e.preventDefault();
    relay((b as HTMLElement).getAttribute("data-for") as string);
  });
});

// Claim the session on load (POST, so passive unfurl/prefetch GETs never claim
// it). Only on success do we receive the WS base + target and start.
async function start() {
  setStatus("claiming session…", "pending");
  let r: Response;
  try {
    r = await fetch("/takeover/" + TOKEN + "/claim", { method: "POST" });
  } catch {
    setStatus("network error reaching the takeover server", "err");
    return;
  }
  if (r.status === 409) {
    setStatus("This takeover link is already in use by someone else.", "err");
    return;
  }
  if (!r.ok) {
    setStatus("could not claim this link (HTTP " + r.status + ")", "err");
    return;
  }
  let info: any;
  try {
    info = await r.json();
  } catch {
    setStatus("bad claim response", "err");
    return;
  }
  if (!info.wsBase || !info.targetId) {
    setStatus("no session URL returned", "err");
    return;
  }
  // The relay base is a path on this daemon; same host, ws(s) scheme.
  const base = new URL(info.wsBase, location.href);
  base.protocol = base.protocol === "https:" ? "wss:" : "ws:";
  wsBase = base.href.replace(/\/$/, "");
  ctxId = info.ctxId;
  if (info.viewport && info.viewport.width > 0 && info.viewport.height > 0)
    sessionVp = info.viewport;
  canvas.width = sessionVp.width;
  canvas.height = sessionVp.height;
  fitCanvas();
  openTab(info.targetId);
}
start();
