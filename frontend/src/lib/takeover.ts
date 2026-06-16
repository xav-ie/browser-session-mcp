// Takeover client: connects straight to Chrome's DevTools WebSocket, renders the
// screencast, forwards input, manages tabs, and relays vault autofill — all over
// CDP. The daemon only serves this page + the /claim and /done endpoints.

// Token comes from the URL (/takeover/<token>) — no server-side templating.
const TOKEN =
  (location.pathname.match(/\/takeover\/([0-9a-f]{32})/) || [])[1] || "";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const ctx = canvas.getContext("2d") as CanvasRenderingContext2D;
const statusEl = document.getElementById("status") as HTMLElement;
const doneBtn = document.getElementById("done") as HTMLButtonElement;
const tabsEl = document.getElementById("tabs") as HTMLElement;
const wrapEl = document.getElementById("wrap") as HTMLElement;
const overlayEl = document.getElementById("overlay") as HTMLElement;
const urlEl = document.getElementById("url") as HTMLInputElement;

let nextId = 1;
let lastMeta: any = null; // most recent screencastFrame metadata (for coord mapping)
let ws: WebSocket | null = null;
let wsBase: string | null = null; // e.g. wss://chrome.<base>
let ctxId: string | null = null; // this session's browserContextId — tabs filtered to it
let currentTarget: string | null = null; // targetId currently screencast
let getTargetsId: number | null = null;
let createTargetId: number | null = null;
let urlQueryId: number | null = null;
let navHistoryId: number | null = null; // pending Page.getNavigationHistory id
let refreshTimer: any = null;
let resizeTimer: any = null;
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
  reload: svg(
    16,
    '<polyline points="23 4 23 10 17 10"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/>',
  ),
  plus: svg(14, '<path d="M12 5v14M5 12h14"/>'),
  close: svg(13, '<path d="M18 6 6 18M6 6l12 12"/>'),
  enter: svg(
    14,
    '<path d="M9 10l-4 4 4 4"/><path d="M5 14h11a4 4 0 0 0 4-4V6"/>',
  ),
};

function send(method: string, params?: any): number | null {
  if (!ws || ws.readyState !== WebSocket.OPEN) return null;
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params: params || {} }));
  return id;
}
function setStatus(s: string) {
  statusEl.textContent = s;
}

// One consistent viewport for EVERY tab, sized to fit this browser window, so
// tabs render identically and the remote page isn't tiny or oversized.
function viewSize() {
  const w = Math.max(
    900,
    Math.min(1680, Math.round(document.documentElement.clientWidth)),
  );
  const top = wrapEl ? wrapEl.getBoundingClientRect().top : 120;
  const h = Math.max(600, Math.round(window.innerHeight - top));
  return { w, h };
}
function applyViewport() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const { w, h } = viewSize();
  send("Emulation.setDeviceMetricsOverride", {
    width: w,
    height: h,
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
    maxWidth: w,
    maxHeight: h,
  });
}

// --- vault relay ----------------------------------------------------------
function relay(id: string) {
  const el = document.getElementById(id) as HTMLInputElement | null;
  const text = (el && el.value) || "";
  if (!text) {
    setStatus("nothing to relay — autofill the box first, then click again");
    return;
  }
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    setStatus("not connected");
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
  setStatus("relayed " + id.slice(1) + " into the focused field");
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
  const plus = document.createElement("button");
  plus.className = "tab newtab";
  plus.innerHTML = ICONS.plus;
  plus.title = "Open a new tab";
  plus.addEventListener("click", () => {
    createTargetId = send("Target.createTarget", {
      url: "about:blank",
      browserContextId: ctxId,
    });
  });
  tabsEl.appendChild(plus);
}

function onMessage(ev: MessageEvent) {
  let msg: any;
  try {
    msg = JSON.parse(ev.data);
  } catch {
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
    setStatus("connected");
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
  ws.onclose = () => {};
  ws.onerror = () =>
    setStatus("connection error — is chrome.<base> reachable?");
  ws.onmessage = onMessage;
}

// --- input forwarding -----------------------------------------------------
function toPage(ev: { clientX: number; clientY: number }) {
  const rect = canvas.getBoundingClientRect();
  const sx = canvas.width / rect.width;
  const sy = canvas.height / rect.height;
  const px = (ev.clientX - rect.left) * sx;
  const py = (ev.clientY - rect.top) * sy;
  const scale = (lastMeta && lastMeta.pageScaleFactor) || 1;
  const offTop = (lastMeta && lastMeta.offsetTop) || 0;
  return { x: px / scale, y: (py - offTop) / scale };
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
canvas.addEventListener("keydown", (e) => {
  e.preventDefault();
  keyEvent("keyDown", e);
});
canvas.addEventListener("keyup", (e) => {
  e.preventDefault();
  keyEvent("keyUp", e);
});

// Re-fit the remote viewport when the takeover window resizes (debounced).
window.addEventListener("resize", () => {
  if (resizeTimer) clearTimeout(resizeTimer);
  resizeTimer = setTimeout(applyViewport, 250);
});

// --- address bar + back/forward/reload ------------------------------------
function navTo() {
  let u = urlEl.value.trim();
  if (!u) return;
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(u)) u = "https://" + u;
  send("Page.navigate", { url: u });
  urlEl.blur();
}
const backBtn = document.getElementById("back") as HTMLButtonElement;
const fwdBtn = document.getElementById("fwd") as HTMLButtonElement;
const reloadBtn = document.getElementById("reload") as HTMLButtonElement;
backBtn.innerHTML = ICONS.back;
fwdBtn.innerHTML = ICONS.fwd;
reloadBtn.innerHTML = ICONS.reload;
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
reloadBtn.addEventListener("click", () => send("Page.reload"));

// --- console-capture (CDP) toggle -----------------------------------------
// Tells the listener (via the daemon) to enable/disable the Runtime domain for
// this session. OFF = stealthier (removes the CDP console side-channel); ON =
// console logging works. The label/colour reflects state.
const captureBtn = document.getElementById("capture") as HTMLButtonElement;
function setCaptureLabel(on: boolean) {
  captureBtn.dataset.on = on ? "1" : "0";
  captureBtn.textContent = on ? "console capture: on" : "console capture: off";
}
async function refreshCapture() {
  try {
    const r = await fetch("/takeover/" + TOKEN + "/capture");
    if (r.ok) setCaptureLabel((await r.json()).captureOn);
  } catch {
    /* leave default */
  }
}
captureBtn.addEventListener("click", async () => {
  const next = captureBtn.dataset.on !== "1";
  try {
    const r = await fetch(
      "/takeover/" + TOKEN + "/capture/" + (next ? "on" : "off"),
      {
        method: "POST",
      },
    );
    if (r.ok) setCaptureLabel(next);
  } catch {
    /* ignore */
  }
});
refreshCapture();
urlEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    navTo();
  }
});

// --- done -----------------------------------------------------------------
doneBtn.addEventListener("click", async () => {
  doneBtn.disabled = true;
  setStatus("handing back to agent…");
  try {
    await fetch("/takeover/" + TOKEN + "/done", { method: "POST" });
    if (ws) {
      try {
        send("Page.stopScreencast");
      } catch {}
      ws.close();
    }
    setStatus("done — you can close this tab");
    overlayEl.textContent =
      "Handed back to the agent — this session is no longer interactive. You can close this tab.";
    overlayEl.style.display = "flex";
  } catch {
    setStatus("failed to signal done — try again");
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
  setStatus("claiming session…");
  let r: Response;
  try {
    r = await fetch("/takeover/" + TOKEN + "/claim", { method: "POST" });
  } catch {
    setStatus("network error reaching the takeover server");
    return;
  }
  if (r.status === 409) {
    setStatus("This takeover link is already in use by someone else.");
    return;
  }
  if (!r.ok) {
    setStatus("could not claim this link (HTTP " + r.status + ")");
    return;
  }
  let info: any;
  try {
    info = await r.json();
  } catch {
    setStatus("bad claim response");
    return;
  }
  if (!info.wsBase || !info.targetId) {
    setStatus("no session URL returned");
    return;
  }
  wsBase = info.wsBase;
  ctxId = info.ctxId;
  openTab(info.targetId);
}
start();
