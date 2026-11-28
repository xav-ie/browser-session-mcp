// The guest's side panel: shows the grant, its state and any pending
// approval, and sends the guest's choices to background.js. It enforces
// nothing itself. Everything from the operator is written with textContent,
// never as HTML.

const $ = (id) => document.getElementById(id);
const port = chrome.runtime.connect({ name: "panel" });
let state = null;

port.onMessage.addListener((m) => {
  if (m.type === "state") {
    state = m.state;
    render();
  } else if (m.type === "log") {
    renderLog(m.entries);
  }
});

function show(view) {
  for (const v of ["idle", "review", "live", "ended"]) $(`view-${v}`).hidden = v !== view;
}

function banner(cls, title, sub) {
  $("banner").className = `banner ${cls}`;
  $("banner-title").textContent = title;
  $("banner-sub").textContent = sub;
}

function left(ms) {
  if (ms <= 0) return "expired";
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function render() {
  const s = state;
  $("error").hidden = !s.error;
  $("error").textContent = s.error ?? "";
  const g = s.grant;
  switch (s.status) {
    case "idle":
    case "loading":
      banner("idle", "No agent", "Nothing is attached to your browser.");
      $("load").disabled = s.status === "loading";
      return show("idle");
    case "review":
    case "connecting":
      banner("review", "Review this request", "Nothing is connected yet.");
      $("g-requester").textContent = g.requester;
      $("g-purpose").textContent = g.purpose;
      $("g-relay").textContent = s.relay;
      $("g-start").textContent = g.startUrl;
      $("g-patterns").replaceChildren(
        ...g.allowedUrlPatterns.map((p) => Object.assign(document.createElement("li"), { textContent: p })),
      );
      $("g-expiry").textContent = `${Math.round(g.expiresInMs / 60000)} minutes from loading the link, then the agent is cut off`;
      if (!document.querySelector('input[name="mode"]:checked')) {
        document.querySelector(`input[name="mode"][value="${g.approvalMode}"]`).checked = true;
      }
      $("g-mode-asked").textContent =
        g.approvalMode === "ask" ? "The operator asked for: ask each time." : "The operator asked for: don't ask.";
      $("accept").disabled = s.status === "connecting";
      $("accept").textContent = s.status === "connecting" ? "Connecting…" : "Accept";
      return show("review");
    case "active":
    case "paused": {
      const asking = s.approvals.length > 0;
      if (asking) banner("asking", "The agent is waiting for you", "Allow or deny below.");
      else if (s.status === "active") banner("active", "AGENT ACTIVE", "It is driving tabs in the colored group.");
      else banner("paused", "PAUSED", "The agent can't do anything until you resume.");
      $("l-purpose").textContent = g.purpose;
      $("l-mode").textContent = s.approvalMode === "ask" ? "Asks before every click, Enter and Space" : "Doesn't ask";
      $("l-tabs").textContent = String(s.tabs);
      $("l-outofscope").hidden = s.outOfScope.length === 0;
      $("l-outofscope").textContent = `Agent locked out of a tab outside the grant: ${s.outOfScope.join(", ")}`;
      $("pause").hidden = s.status !== "active";
      $("resume").hidden = s.status !== "paused";
      renderApprovals(s.approvals);
      tick();
      return show("live");
    }
    case "ended":
      banner("ended", "Agent gone", "Nothing is attached to your browser.");
      $("e-reason").textContent = `The grant ended: ${s.endReason}.`;
      return show("ended");
  }
}

function tick() {
  if (state && (state.status === "active" || state.status === "paused")) {
    $("l-countdown").textContent = left(state.expiresAt - Date.now());
  }
}
setInterval(tick, 1000);

const KIND = {
  click: "The agent wants to click",
  enter: "The agent wants to press Enter",
  space: "The agent wants to press Space",
};

function renderApprovals(list) {
  const nodes = list.map((a) => {
    const box = document.createElement("div");
    box.className = "approval";
    const h = document.createElement("h3");
    h.textContent = KIND[a.kind] ?? "The agent wants to act";
    const what = document.createElement("p");
    what.textContent = a.target ? `On: ${a.target}` : "On: (couldn't tell what is there)";
    const url = document.createElement("p");
    url.className = "mono";
    url.textContent = a.url;
    box.append(h, what);
    if (a.at) {
      const at = document.createElement("p");
      at.className = "note";
      at.textContent = `At ${a.at.x}, ${a.at.y} in the page (circled):`;
      box.append(at);
    }
    if (a.thumb) {
      const wrap = document.createElement("span");
      wrap.className = "thumb";
      const img = document.createElement("img");
      img.src = a.thumb;
      img.alt = "What is under the pointer";
      wrap.append(img);
      box.append(wrap);
    }
    box.append(url);
    const row = document.createElement("div");
    row.className = "row";
    const allow = Object.assign(document.createElement("button"), { textContent: "Allow", className: "primary" });
    const deny = Object.assign(document.createElement("button"), { textContent: "Deny", className: "danger" });
    allow.onclick = () => port.postMessage({ type: "approve", id: a.id, allow: true });
    deny.onclick = () => port.postMessage({ type: "approve", id: a.id, allow: false });
    row.append(allow, deny);
    box.append(row);
    return box;
  });
  $("approvals").replaceChildren(...nodes);
}

function renderLog(entries) {
  $("log").replaceChildren(
    ...entries
      .slice(-300)
      .reverse()
      .map((e) => {
        const li = document.createElement("li");
        const { at, ...rest } = e;
        li.textContent = `${at.slice(11, 19)} ${JSON.stringify(rest)}`;
        return li;
      }),
  );
  lastLog = entries;
}
let lastLog = [];

$("load").onclick = () => port.postMessage({ type: "load", link: $("link").value });
$("link").onkeydown = (e) => e.key === "Enter" && $("load").click();
$("accept").onclick = () =>
  port.postMessage({ type: "accept", approvalMode: document.querySelector('input[name="mode"]:checked')?.value });
$("decline").onclick = () => port.postMessage({ type: "decline" });
$("pause").onclick = () => port.postMessage({ type: "pause" });
$("resume").onclick = () => port.postMessage({ type: "resume" });
$("end").onclick = () => port.postMessage({ type: "end" });
$("reset").onclick = () => {
  for (const r of document.querySelectorAll('input[name="mode"]')) r.checked = false;
  port.postMessage({ type: "reset" });
};
$("log-box").ontoggle = () => $("log-box").open && port.postMessage({ type: "getLog" });
$("log-clear").onclick = () => port.postMessage({ type: "clearLog" });
$("log-export").onclick = () => {
  const blob = new Blob([JSON.stringify(lastLog, null, 2)], { type: "application/json" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `guest-grant-log-${new Date().toISOString().slice(0, 19).replace(/:/g, "")}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
};
// Keep the open log current.
setInterval(() => $("log-box").open && port.postMessage({ type: "getLog" }), 2000);
