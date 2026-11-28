// Guest grant service worker: runs a grant the guest accepted.
//
// What it does, in order:
//   1. The side panel hands it a grant link. It fetches the grant from the
//      relay, checks it with policy.validateGrant, and shows it to the guest.
//      Nothing is attached and no socket is open yet.
//   2. On Accept it opens an outbound WebSocket to the relay, opens the start
//      URL in a new tab inside a tab group of its own, and from then on
//      answers the relay's commands, one at a time, for that group only.
//   3. Every command goes through policy.checkCommand (allowlist and
//      parameter rebuild), the URL scope, and, in "ask" mode, the guest's
//      Allow/Deny. Only then is it sent with chrome.debugger to one tab of
//      the group. Anything else is refused and logged.
//   4. End now, expiry, closing the group or its tabs, Chrome's own "Cancel"
//      on the debugging banner, the relay revoking it, or the socket dropping
//      all end the grant: the debugger detaches from every tab and nothing is
//      forwarded again. The tabs stay, since they are the guest's.
//
// The relay only ever learns what this file sends back: command results for
// in-scope pages, and the origin (never the full URL) of a page it blocked.

import * as policy from "./policy.js";

const APPROVAL_TIMEOUT_MS = 90_000;
const PING_MS = 20_000;
const CONNECT_TIMEOUT_MS = 10_000;
const LOG_MAX = 2000;
// A CDP command Chrome never answers (a screenshot of a huge page can hang)
// must not hold up every command after it.
const CDP_TIMEOUT_MS = 25_000;
const GROUP_PREFIX = "Agent: ";

/** Everything about the current grant. Mirrored to storage.session (see persist). */
let S = fresh();
function fresh() {
  return {
    // idle | loading | review | connecting | active | paused | ended
    status: "idle",
    error: null,
    link: null,
    grant: null,
    loadedAt: 0,
    patterns: [],
    approvalMode: "ask",
    expiresAt: 0,
    endReason: null,
    groupId: null,
    // Tabs the agent may drive: the one the grant opened and tabs opened from
    // them. Being in the group is required but not enough, so a tab the guest
    // drags into the group never becomes the agent's.
    agentTabs: new Set(),
    currentTab: null,
    attached: new Set(),
    // tabId -> origin, for agent tabs whose page is outside the grant.
    outOfScope: new Map(),
    approvals: new Map(),
  };
}

let ws = null;
let timers = [];
let queue = Promise.resolve();
const panels = new Set();
let approvalWindowId = null;

// ---- persistence and restarts ----------------------------------------------

/**
 * Chrome may stop this service worker; a grant can't survive that, since its
 * socket dies with it. So the state is kept in storage.session only to clean
 * up after a restart: detach the debugger and end the grant.
 */
function persist() {
  const saved = {
    status: S.status,
    grant: S.grant,
    link: S.link,
    endReason: S.endReason,
    error: S.error,
    groupId: S.groupId,
    agentTabs: [...S.agentTabs],
    attached: [...S.attached],
  };
  chrome.storage.session.set({ saved }).catch(() => {});
}

const ready = (async () => {
  const { saved } = await chrome.storage.session.get("saved");
  // Never a live grant at startup: drop any scope rules left behind.
  await clearNetRules();
  if (!saved) return;
  if (saved.status === "active" || saved.status === "paused" || saved.status === "connecting") {
    for (const tabId of saved.attached ?? []) await chrome.debugger.detach({ tabId }).catch(() => {});
    if (saved.groupId != null) await markGroup(saved.groupId, "gone").catch(() => {});
    S.status = "ended";
    S.grant = saved.grant;
    S.endReason = "the extension restarted, which closed the connection to the agent";
    log({ kind: "grant", event: "ended", reason: S.endReason });
    persist();
  } else if (saved.status === "ended") {
    Object.assign(S, { status: "ended", grant: saved.grant, endReason: saved.endReason });
  }
})();

chrome.runtime.onInstalled.addListener(setupPanel);
chrome.runtime.onStartup.addListener(setupPanel);
function setupPanel() {
  chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch(() => {});
}

// ---- the action log ------------------------------------------------------------

let logBuf = [];
let logFlush = null;
/** Append to the guest's local log: commands, denials, approvals, grant events. */
function log(entry) {
  logBuf.push({ at: new Date().toISOString(), ...entry });
  if (!logFlush) logFlush = setTimeout(flushLog, 300);
}
async function flushLog() {
  logFlush = null;
  const add = logBuf;
  logBuf = [];
  const { log: old = [] } = await chrome.storage.local.get("log");
  await chrome.storage.local.set({ log: old.concat(add).slice(-LOG_MAX) });
}
async function readLog() {
  await flushLog();
  const { log: entries = [] } = await chrome.storage.local.get("log");
  return entries;
}

// ---- the side panel ------------------------------------------------------------

chrome.runtime.onConnect.addListener(async (port) => {
  if (port.name !== "panel" || port.sender?.id !== chrome.runtime.id) return;
  await ready;
  panels.add(port);
  port.onDisconnect.addListener(() => panels.delete(port));
  port.onMessage.addListener((m) => onPanel(port, m).catch((e) => fail(e)));
  port.postMessage({ type: "state", state: publicState() });
});

/** What the panel shows. No secrets: there are none here. */
function publicState() {
  return {
    status: S.status,
    error: S.error,
    grant: S.grant,
    relay: S.link?.origin ?? null,
    approvalMode: S.approvalMode,
    expiresAt: S.expiresAt,
    endReason: S.endReason,
    tabs: S.agentTabs.size,
    outOfScope: [...S.outOfScope.values()],
    approvals: [...S.approvals.values()].map(({ resolve, ...a }) => a),
  };
}

function push() {
  const msg = { type: "state", state: publicState() };
  for (const p of panels) p.postMessage(msg);
  const pending = S.approvals.size;
  const text = pending ? "!" : S.status === "active" ? "ON" : S.status === "paused" ? "II" : "";
  chrome.action.setBadgeText({ text }).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ color: pending ? "#d93025" : "#7c4dbb" }).catch(() => {});
}

function fail(e) {
  S.error = String(e?.message ?? e);
  push();
}

async function onPanel(port, m) {
  S.error = null;
  switch (m?.type) {
    case "load":
      return loadGrant(m.link);
    case "accept":
      return accept(m.approvalMode === "auto" ? "auto" : "ask");
    case "decline":
      return decline();
    case "pause":
      return pause();
    case "resume":
      return resume();
    case "end":
      return end("the guest pressed End now");
    case "reset":
      if (S.status === "ended" || S.status === "review" || S.status === "idle") {
        S = fresh();
        persist();
        push();
      }
      return;
    case "approve": {
      const a = S.approvals.get(m.id);
      if (a) a.resolve(m.allow === true);
      return;
    }
    case "getLog":
      return port.postMessage({ type: "log", entries: await readLog() });
    case "clearLog":
      await flushLog();
      await chrome.storage.local.set({ log: [] });
      return port.postMessage({ type: "log", entries: [] });
  }
}

// ---- grant lifecycle -----------------------------------------------------------

async function loadGrant(raw) {
  if (["connecting", "active", "paused"].includes(S.status)) throw new Error("end the current grant first");
  const link = policy.parseGrantLink(raw);
  S = fresh();
  S.status = "loading";
  push();
  try {
    // No cookies, no cache: the relay learns nothing about the guest here.
    const r = await fetch(link.grantUrl, { credentials: "omit", cache: "no-store", redirect: "error" });
    if (!r.ok) throw new Error(`the relay answered ${r.status}: ${(await r.text()).slice(0, 200)}`);
    S.grant = policy.validateGrant(await r.json());
    S.link = link;
    S.loadedAt = Date.now();
    S.approvalMode = S.grant.approvalMode;
    S.status = "review";
  } catch (e) {
    S = fresh();
    S.error = `Could not load that grant: ${e.message}`;
  }
  persist();
  push();
}

async function decline() {
  if (S.status !== "review") return;
  const { declineUrl } = S.link;
  log({ kind: "grant", event: "declined", purpose: S.grant.purpose });
  S = fresh();
  persist();
  push();
  await fetch(declineUrl, { method: "POST", credentials: "omit", cache: "no-store" }).catch(() => {});
}

async function accept(mode) {
  if (S.status !== "review") return;
  S.patterns = S.grant.allowedUrlPatterns.map(policy.parsePattern);
  S.approvalMode = mode;
  S.expiresAt = S.loadedAt + S.grant.expiresInMs;
  if (Date.now() >= S.expiresAt) {
    S = fresh();
    S.error = "That grant expired before it was accepted.";
    return push();
  }
  S.status = "connecting";
  push();
  try {
    await openSocket();
  } catch (e) {
    S.status = "review";
    S.error = `Could not reach the operator's relay: ${e.message}`;
    return push();
  }
  const tab = await chrome.tabs.create({ url: "about:blank", active: true });
  S.agentTabs.add(tab.id);
  S.currentTab = tab.id;
  S.groupId = await chrome.tabs.group({ tabIds: [tab.id] });
  await markGroup(S.groupId, "active");
  await syncNetRules();
  S.status = "active";
  log({
    kind: "grant",
    event: "accepted",
    purpose: S.grant.purpose,
    requester: S.grant.requester,
    allowedUrlPatterns: S.grant.allowedUrlPatterns,
    approvalMode: mode,
    expiresAt: new Date(S.expiresAt).toISOString(),
  });
  send({ method: "Guest.hello", params: { version: 1, approvalMode: mode, expiresInMs: S.expiresAt - Date.now() } });
  timers.push(setInterval(() => send({ method: "Guest.ping" }), PING_MS));
  timers.push(setInterval(() => Date.now() >= S.expiresAt && end("the grant expired"), 1000));
  persist();
  push();
  await chrome.tabs.update(tab.id, { url: S.grant.startUrl });
}

function openSocket() {
  return new Promise((resolve, reject) => {
    const sock = new WebSocket(S.link.socketUrl);
    const t = setTimeout(() => {
      sock.close();
      reject(new Error("timed out"));
    }, CONNECT_TIMEOUT_MS);
    sock.onopen = () => {
      clearTimeout(t);
      ws = sock;
      resolve();
    };
    sock.onerror = () => {};
    sock.onclose = (ev) => {
      clearTimeout(t);
      if (ws !== sock) return reject(new Error(ev.reason || `closed (${ev.code})`));
      ws = null;
      const why = ev.reason ? `: ${ev.reason.slice(0, 200)}` : "";
      end(`the connection to the operator closed${why}`);
    };
    sock.onmessage = (ev) => {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (typeof msg?.id !== "number") {
        if (msg?.method === "Guest.revoke") end("the operator revoked the grant");
        return;
      }
      // One command at a time, in order: a press, then its release.
      queue = queue.then(() => handle(msg)).catch((e) => send({ id: msg.id, error: { code: -32000, message: String(e?.message ?? e) } }));
    };
  });
}

function send(obj) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
}

async function pause() {
  if (S.status !== "active") return;
  S.status = "paused";
  denyAllApprovals("the guest paused the grant");
  await detachAll();
  await markGroup(S.groupId, "paused");
  log({ kind: "grant", event: "paused" });
  send({ method: "Guest.paused" });
  persist();
  push();
}

async function resume() {
  if (S.status !== "paused") return;
  S.status = "active";
  await markGroup(S.groupId, "active");
  log({ kind: "grant", event: "resumed" });
  send({ method: "Guest.resumed" });
  persist();
  push();
}

/** End the grant for good. Safe to call more than once. */
async function end(reason) {
  if (!["connecting", "active", "paused"].includes(S.status)) return;
  S.status = "ended";
  S.endReason = reason;
  denyAllApprovals("the grant ended");
  for (const t of timers) clearInterval(t);
  timers = [];
  const sock = ws;
  ws = null;
  if (sock && sock.readyState === WebSocket.OPEN) {
    sock.send(JSON.stringify({ method: "Guest.ended", params: { reason } }));
    sock.close(1000, "grant ended");
  }
  await detachAll();
  await clearNetRules();
  if (S.groupId != null) await markGroup(S.groupId, "gone").catch(() => {});
  log({ kind: "grant", event: "ended", reason });
  persist();
  push();
}

async function detachAll() {
  for (const tabId of S.attached) await chrome.debugger.detach({ tabId }).catch(() => {});
  S.attached.clear();
}

/** Name and color the agent's tab group so its state shows in the tab strip. */
async function markGroup(groupId, state) {
  const purpose = (S.grant?.purpose ?? "").slice(0, 40);
  const look = {
    active: { title: `${GROUP_PREFIX}${purpose}`, color: "purple" },
    paused: { title: `Paused: ${purpose}`, color: "yellow" },
    gone: { title: "Agent gone", color: "grey" },
  }[state];
  await chrome.tabGroups.update(groupId, look);
}

// ---- commands from the relay ------------------------------------------------------

function refuse(id, message) {
  send({ id, error: { code: -32000, message: `guest extension: ${message}` } });
}

async function handle(msg) {
  const { id, method } = msg;
  if (S.status === "paused") return refuse(id, "the guest paused the grant; calls fail until they resume it");
  if (S.status !== "active") return refuse(id, "the grant is not active");
  if (Date.now() >= S.expiresAt) {
    await end("the grant expired");
    return refuse(id, "the grant expired");
  }
  const c = policy.checkCommand(method, msg.params);
  if (!c.ok) {
    log({ kind: "denied", method: String(method).slice(0, 80), why: c.why });
    return refuse(id, c.why);
  }
  const tabId = S.currentTab;
  const tab = tabId == null ? null : await chrome.tabs.get(tabId).catch(() => null);
  if (!tab || !S.agentTabs.has(tabId)) return refuse(id, "the agent has no tab left in its group");

  if (c.local) return send({ id, result: await local(c, tab) });

  // Where the tab is now. Only a navigation may start from a page outside the
  // grant (to get back in); nothing reads or clicks there.
  const here = policy.checkUrl(tab.pendingUrl || tab.url, S.patterns);
  if (!c.navigates && (!here.ok || S.outOfScope.has(tabId))) {
    log({ kind: "denied", method, why: "the tab is outside the grant" });
    return refuse(id, `the tab is on ${policy.originOnly(tab.url)}, outside the grant (${here.why ?? "out of scope"})`);
  }
  if (c.navigates) {
    const v = policy.checkUrl(c.params.url, S.patterns);
    if (!v.ok) {
      log({ kind: "denied", method, url: c.params.url, why: v.why });
      return refuse(id, `${policy.originOnly(c.params.url)} is blocked: ${v.why}`);
    }
  }

  await ensureAttached(tabId);
  if (c.approval && S.approvalMode === "ask") {
    const allowed = await askGuest(c, tabId, tab);
    if (!allowed) return refuse(id, "the guest denied this action");
    // The page may have changed while the guest decided.
    const now = await chrome.tabs.get(tabId).catch(() => null);
    if (S.status !== "active" || !now || now.url !== tab.url || S.outOfScope.has(tabId)) {
      return refuse(id, "the page changed while the guest was deciding; nothing was sent");
    }
  }
  const result = await send1(tabId, method, c.params);
  log({ kind: "command", method, params: policy.redactForLog(method, c.params) });

  // The page may have left the grant while the command ran: send nothing read
  // from it then.
  if (!c.navigates) {
    const after = await chrome.tabs.get(tabId).catch(() => null);
    if (!after || !policy.checkUrl(after.url, S.patterns).ok || S.outOfScope.has(tabId)) {
      return refuse(id, "the page left the grant's scope, so its result was withheld");
    }
  }
  send({ id, result: result ?? {} });
}

/** Guest.* methods, answered from the tabs API for in-scope agent tabs only. */
async function local(c, tab) {
  if (c.method === "Guest.waitForLoad") {
    const deadline = Date.now() + c.params.timeoutMs;
    while (Date.now() < deadline) {
      const t = await chrome.tabs.get(tab.id).catch(() => null);
      if (!t || t.status === "complete") break;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  const describe = async (tabId) => {
    const t = await chrome.tabs.get(tabId).catch(() => null);
    if (!t) return null;
    const inScope = policy.checkUrl(t.url, S.patterns).ok && !S.outOfScope.has(tabId);
    return {
      tabId,
      current: tabId === S.currentTab,
      status: t.status,
      url: inScope ? t.url : null,
      title: inScope ? t.title : null,
      outOfScope: inScope ? null : policy.originOnly(t.url),
    };
  };
  const tabs = (await Promise.all([...S.agentTabs].map(describe))).filter(Boolean);
  return { ...(await describe(tab.id)), tabs };
}

async function ensureAttached(tabId) {
  if (S.attached.has(tabId)) return;
  await chrome.debugger.attach({ tabId }, "1.3");
  S.attached.add(tabId);
  persist();
}

// ---- asking the guest -------------------------------------------------------------

function denyAllApprovals(why) {
  for (const a of S.approvals.values()) a.resolve(false, why);
}

/**
 * Show a pending click or key to the guest and wait for Allow or Deny (or
 * deny after APPROVAL_TIMEOUT_MS). Shows what is under the pointer, or which
 * element has focus, read from the accessibility tree, plus the page URL.
 */
async function askGuest(c, tabId, tab) {
  const p = c.params;
  const id = crypto.randomUUID();
  const info = { id, kind: c.approval, url: tab.url, target: null, thumb: null, at: null };
  try {
    if (c.method === "Input.dispatchMouseEvent") {
      info.at = { x: Math.round(p.x), y: Math.round(p.y) };
      info.target = await describePoint(tabId, p.x, p.y);
      info.thumb = await thumbnail(tabId, p.x, p.y);
    } else {
      info.target = await describeFocus(tabId);
    }
  } catch {
    // Describing is best effort; the guest still sees the kind and the URL.
  }
  return new Promise((resolve) => {
    const timer = setTimeout(() => done(false, "no answer in time"), APPROVAL_TIMEOUT_MS);
    const done = (allow, why) => {
      clearTimeout(timer);
      if (!S.approvals.delete(id)) return;
      log({ kind: "approval", action: info.kind, target: info.target, url: info.url, allowed: allow, why: why ?? null });
      push();
      resolve(allow);
    };
    S.approvals.set(id, { ...info, resolve: done });
    push();
    showApprovalSurface();
  });
}

/** Open a small window with the panel when no side panel is open to ask in. */
async function showApprovalSurface() {
  if (panels.size > 0) return;
  if (approvalWindowId != null) {
    await chrome.windows.update(approvalWindowId, { focused: true }).catch(() => (approvalWindowId = null));
    if (approvalWindowId != null) return;
  }
  const w = await chrome.windows.create({ url: "panel.html?window=1", type: "popup", width: 440, height: 680, focused: true });
  approvalWindowId = w.id;
}
chrome.windows.onRemoved.addListener((id) => {
  if (id === approvalWindowId) approvalWindowId = null;
});

/** One CDP command to one agent tab, given up on after CDP_TIMEOUT_MS. */
function send1(tabId, method, params = {}) {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`Chrome didn't answer ${method} within ${CDP_TIMEOUT_MS / 1000} s`)), CDP_TIMEOUT_MS);
  });
  return Promise.race([chrome.debugger.sendCommand({ tabId }, method, params), late]).finally(() => clearTimeout(timer));
}

/** How far the page is scrolled: clips and hit tests take page coordinates. */
async function scrollOffset(tabId) {
  const m = await send1(tabId, "Page.getLayoutMetrics");
  const vp = m.cssVisualViewport ?? m.visualViewport ?? {};
  return { x: vp.pageX ?? 0, y: vp.pageY ?? 0 };
}

/** `role "name"` of the nearest meaningful node at a viewport point. */
async function describePoint(tabId, x, y) {
  await send1(tabId, "DOM.getDocument", { depth: 0 });
  const at = await scrollOffset(tabId);
  const { backendNodeId } = await send1(tabId, "DOM.getNodeForLocation", {
    x: Math.round(at.x + x),
    y: Math.round(at.y + y),
    includeUserAgentShadowDOM: false,
  });
  if (!backendNodeId) return null;
  const { nodes } = await send1(tabId, "Accessibility.getPartialAXTree", { backendNodeId, fetchRelatives: true });
  const byId = new Map(nodes.map((n) => [n.nodeId, n]));
  let n = nodes.find((x) => x.backendDOMNodeId === backendNodeId) ?? nodes[0];
  const boring = new Set(["generic", "none", "StaticText", "InlineTextBox", "LineBreak", "paragraph", "image"]);
  for (let i = 0; n && i < 8; i++) {
    const role = n.role?.value;
    if (!n.ignored && role && !boring.has(role)) break;
    n = byId.get(n.parentId);
  }
  return n ? axLabel(n) : null;
}

/** `role "name"` of the focused node, for a key press. */
async function describeFocus(tabId) {
  const { nodes } = await send1(tabId, "Accessibility.getFullAXTree", {});
  const focused = nodes.filter((n) => n.properties?.some((p) => p.name === "focused" && p.value?.value === true));
  const n = focused.at(-1);
  return n ? axLabel(n) : "nothing focused (the page itself)";
}

function axLabel(n) {
  const role = n.role?.value ?? "element";
  const name = String(n.name?.value ?? "").slice(0, 120);
  return name ? `${role} "${name}"` : role;
}

/** A 320x140 crop of the screen centred on the pointer. */
async function thumbnail(tabId, x, y) {
  const at = await scrollOffset(tabId);
  const { data } = await send1(tabId, "Page.captureScreenshot", {
    format: "png",
    clip: { x: at.x + x - 160, y: at.y + y - 70, width: 320, height: 140, scale: 1 },
  });
  return `data:image/png;base64,${data}`;
}

// ---- keeping the agent inside its tabs and the grant's URLs ------------------------------

function live() {
  return S.status === "active" || S.status === "paused";
}

/** Make a tab opened from an agent tab the agent's too, inside the group. */
async function adopt(tabId) {
  if (!live() || S.agentTabs.has(tabId)) return;
  S.agentTabs.add(tabId);
  try {
    await chrome.tabs.group({ tabIds: [tabId], groupId: S.groupId });
  } catch {
    // A popup window can't hold a tab group: it is not the agent's then.
    S.agentTabs.delete(tabId);
    log({ kind: "tab", event: "a popup window opened outside the group; the agent can't use it" });
    return;
  }
  S.currentTab = tabId;
  await syncNetRules();
  const t = await chrome.tabs.get(tabId).catch(() => null);
  if (t) scopeCheck(tabId, t.pendingUrl || t.url);
  log({ kind: "tab", event: "opened from an agent tab; joined the group", tabId });
  persist();
  push();
}

/** A tab stops being the agent's: detach and forget it. */
async function release(tabId, why) {
  if (!S.agentTabs.delete(tabId)) return;
  S.outOfScope.delete(tabId);
  if (S.attached.delete(tabId)) await chrome.debugger.detach({ tabId }).catch(() => {});
  if (S.currentTab === tabId) S.currentTab = [...S.agentTabs].at(-1) ?? null;
  log({ kind: "tab", event: why, tabId });
  if (S.agentTabs.size === 0) return end("the guest closed the agent's tabs");
  await syncNetRules();
  persist();
  push();
}

/**
 * The second layer behind the network rules: every top-level URL change in an
 * agent tab (navigation, redirect, history.pushState, fragment) is checked.
 * Out of scope, the debugger detaches and every command on the tab is refused
 * until it is back in scope; the agent hears only the origin.
 */
async function scopeCheck(tabId, url) {
  if (!live() || !S.agentTabs.has(tabId) || !url) return;
  const v = policy.checkUrl(url, S.patterns);
  if (!v.ok) {
    if (S.outOfScope.has(tabId)) return;
    const origin = policy.originOnly(url);
    S.outOfScope.set(tabId, origin);
    if (S.attached.delete(tabId)) await chrome.debugger.detach({ tabId }).catch(() => {});
    log({ kind: "navigation", event: "left the grant; agent locked out of the tab", url, why: v.why });
    send({ method: "Guest.navigationBlocked", params: { origin, why: v.why } });
    push();
  } else if (S.outOfScope.delete(tabId)) {
    log({ kind: "navigation", event: "back inside the grant", url });
    push();
  }
}

chrome.tabs.onCreated.addListener((tab) => {
  if (tab.openerTabId != null && S.agentTabs.has(tab.openerTabId)) adopt(tab.id);
});
chrome.webNavigation.onCreatedNavigationTarget.addListener((d) => {
  if (S.agentTabs.has(d.sourceTabId)) adopt(d.tabId);
});
chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (!live()) return;
  if (change.groupId !== undefined) {
    if (S.agentTabs.has(tabId) && change.groupId !== S.groupId) {
      release(tabId, "moved out of the group by the guest; no longer the agent's");
    } else if (!S.agentTabs.has(tabId) && change.groupId === S.groupId) {
      // Only tabs the agent opened belong in its group.
      chrome.tabs.ungroup(tabId).catch(() => {});
      log({ kind: "tab", event: "a tab the agent didn't open was put in its group; taken out", tabId });
    }
  }
  if (change.url) scopeCheck(tabId, change.url);
});
chrome.tabs.onActivated.addListener(({ tabId }) => {
  if (S.agentTabs.has(tabId)) S.currentTab = tabId;
});
chrome.tabs.onRemoved.addListener((tabId) => {
  if (S.agentTabs.has(tabId)) release(tabId, "closed");
});
chrome.tabGroups.onRemoved.addListener((group) => {
  if (live() && group.id === S.groupId) end("the guest closed the tab group");
});
for (const ev of ["onBeforeNavigate", "onCommitted", "onHistoryStateUpdated", "onReferenceFragmentUpdated"]) {
  chrome.webNavigation[ev].addListener((d) => {
    if (d.frameId === 0) scopeCheck(d.tabId, d.url);
  });
}
chrome.debugger.onDetach.addListener((source, reason) => {
  if (!S.attached.delete(source.tabId)) return;
  if (reason === "canceled_by_user") end("the guest pressed Cancel on Chrome's debugging banner");
});

// ---- network rules ----------------------------------------------------------------

/**
 * Session rules for the agent's tabs (see policy.js): block every top-level
 * request, allow the grant's patterns, block the denylist above them. A
 * pattern Chrome can't take as a regex is left out, which blocks it: this
 * layer fails closed.
 */
async function syncNetRules() {
  const existing = await chrome.declarativeNetRequest.getSessionRules();
  const removeRuleIds = existing.map((r) => r.id);
  const tabIds = [...S.agentTabs];
  if (!live() && S.status !== "connecting") return chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds });
  if (tabIds.length === 0) return chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds });
  const addRules = [];
  let id = 1;
  const condition = (extra) => ({ resourceTypes: ["main_frame"], tabIds, ...extra });
  addRules.push({ id: id++, priority: 1, action: { type: "block" }, condition: condition({}) });
  for (const p of S.patterns) {
    const regexFilter = policy.patternToRegexFilter(p);
    if (!(await regexOk(regexFilter))) {
      log({ kind: "rules", event: "pattern too complex for Chrome's network rules; it stays blocked", pattern: p.source });
      continue;
    }
    addRules.push({
      id: id++,
      priority: 2,
      action: { type: "allow" },
      condition: condition({ regexFilter, isUrlFilterCaseSensitive: true }),
    });
  }
  for (const regexFilter of policy.denyRegexFilters()) {
    if (!(await regexOk(regexFilter))) continue;
    addRules.push({
      id: id++,
      priority: 3,
      action: { type: "block" },
      condition: condition({ regexFilter, isUrlFilterCaseSensitive: false }),
    });
  }
  await chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds, addRules });
}

const regexCache = new Map();
async function regexOk(regex) {
  if (!regexCache.has(regex)) {
    const r = await chrome.declarativeNetRequest.isRegexSupported({ regex, isCaseSensitive: false });
    regexCache.set(regex, r.isSupported);
  }
  return regexCache.get(regex);
}

async function clearNetRules() {
  const existing = await chrome.declarativeNetRequest.getSessionRules();
  if (existing.length) {
    await chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: existing.map((r) => r.id) });
  }
}
