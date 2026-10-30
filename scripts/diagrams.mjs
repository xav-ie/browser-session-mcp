#!/usr/bin/env node
// Generates the docs' diagrams from docs/diagrams/palette.json (a light and a dark theme) and the
// layouts below. It writes docs/diagrams/<name>.svg (light) and <name>-dark.svg, and fills every
//   <!-- diagram:NAME --> … <!-- /diagram:NAME -->
// block in the docs with a <picture> holding the diagram and its alt text.
// Run from anywhere: node scripts/diagrams.mjs
// CI runs it and fails if anything it writes differs from what's committed.
import { mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (file) => readFileSync(path.join(root, file), "utf8");
const write = (file, text) => writeFileSync(path.join(root, file), text);
const DIR = "docs/diagrams";
const palette = JSON.parse(read(`${DIR}/palette.json`));
const DOCS = ["README.md", "docs/how-it-works.md", "docs/workflows.md", "docs/development.md"];

// --- primitives -------------------------------------------------------------------
const W = 1062;
const M = 24;
const SIZE = { title: 16, sub: 13.5, label: 13, mono: 12.5 };
const FONT = `-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif`;
const MONO = `ui-monospace, SFMono-Regular, Menlo, Consolas, monospace`;

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const n = (v) => Math.round(v * 10) / 10;
// Deterministic text width estimates, so layout needs no browser.
const textWidth = (s, size) => s.length * size * 0.54;
// Greedy word wrap to a pixel width.
const wrap = (s, size, width) =>
  s.split(" ").reduce(
    (out, word) => {
      const last = out.at(-1);
      if (last && textWidth(`${last} ${word}`, size) > width) out.push(word);
      else out[out.length - 1] = last ? `${last} ${word}` : word;
      return out;
    },
    [""],
  );

// Themes apply through CSS classes, so every diagram body is written once.
// `key` marks browser-session's own parts; `note` is a remark spanning lifelines.
const style = (c) => `<style>
  text { font-family: ${FONT}; fill: ${c.text}; }
  .bg { fill: ${c.bg}; }
  .box { fill: ${c.surface}; stroke: ${c.line}; stroke-width: 2; }
  .key { fill: ${c.key}; fill-opacity: .12; stroke: ${c.key}; stroke-width: 2.5; }
  .end { fill: ${c.border}; fill-opacity: .14; stroke: ${c.border}; stroke-width: 2; }
  .note { fill: ${c.note}; stroke: ${c.noteLine}; stroke-width: 1.5; }
  .frame { fill: ${c.surface}; stroke: ${c.line}; stroke-width: 1.5; }
  .group { fill: none; stroke: ${c.muted}; stroke-width: 1.5; stroke-dasharray: 6 5; }
  .title { font-size: ${SIZE.title}px; font-weight: 650; }
  .sub { font-size: ${SIZE.sub}px; fill: ${c.muted}; }
  .label { font-size: ${SIZE.label}px; font-weight: 550; fill: ${c.border}; }
  .mono { font-family: ${MONO}; font-size: ${SIZE.mono}px; }
  .edge { fill: none; stroke: ${c.border}; stroke-width: 2; marker-end: url(#arrow); }
  .both { marker-start: url(#arrow); }
  .ret { stroke-dasharray: 7 5; }
  .life { stroke: ${c.line}; stroke-width: 2; stroke-dasharray: 4 5; }
  .tag { fill: ${c.bg}; }
  .head { fill: ${c.border}; }
</style>`;

const svg = (h, body) =>
  ["light", "dark"].map(
    (theme) =>
      `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${n(h)}" width="${W}" height="${n(h)}" role="img">` +
      `${style(palette[theme])}<defs><marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path class="head" d="M0 0L10 5L0 10z"/></marker></defs>` +
      `<rect class="bg" width="${W}" height="${n(h)}"/>${body}</svg>\n`,
  );

const text = (cls, x, y, s, anchor = "start") =>
  `<text class="${cls}" x="${n(x)}" y="${n(y)}"${anchor === "start" ? "" : ` text-anchor="${anchor}"`}>${esc(s)}</text>`;
const rect = (cls, x, y, w, h, r) =>
  `<rect class="${cls}" x="${n(x)}" y="${n(y)}" width="${n(w)}" height="${n(h)}" rx="${r}"/>`;

// Lines of text centered on (x, y): each a [string, class] pair.
const lines = (x, y, all) => {
  const height = all.reduce((sum, [, cls]) => sum + SIZE[cls] * 1.35, 0);
  let top = y - height / 2;
  return all
    .map(([s, cls]) => {
      top += SIZE[cls] * 1.35;
      return text(cls, x, top - SIZE[cls] * 0.35, s, "middle");
    })
    .join("");
};

// A box: kind is box (plain), key (browser-session's parts), end (a result) or frame.
// The first line is its title; the rest wrap to the box as subtitles. `mono` lines sit below
// them in a frame.
const box = ({ x, y, w, h, text: [title, ...subs], kind = "box", mono }) => {
  const all = [
    ...wrap(title, SIZE.title, w - 24).map((s) => [s, "title"]),
    ...subs.flatMap((s) => wrap(s, SIZE.sub, w - 24)).map((s) => [s, "sub"]),
  ];
  if (!mono) return rect(kind, x, y, w, h, 14) + lines(x + w / 2, y + h / 2, all);
  const fh = mono.length * 19 + 14;
  const top = y + h - fh - 14;
  return (
    rect(kind, x, y, w, h, 14) +
    lines(x + w / 2, y + (top - y) / 2 + 2, all) +
    rect("frame", x + 14, top, w - 28, fh, 8) +
    mono
      .map((s, i) => text("mono", x + 26, top + 22 + i * 19, s).replace("<text", `<text xml:space="preserve"`))
      .join("")
  );
};

// A label on a line, with a background so the line doesn't run through it.
const label = (x, y, s, anchor = "middle") => {
  const w = textWidth(s, SIZE.label) + 12;
  const left = anchor === "middle" ? x - w / 2 : anchor === "start" ? x - 6 : x - w + 6;
  return rect("tag", left, y - 12, w, 19, 5) + text("label", x, y + 2, s, anchor);
};

const arrow = (points, cls = "edge") =>
  `<path class="${cls}" d="M${points.map(([x, y]) => `${n(x)} ${n(y)}`).join("L")}"/>`;

const at = (b) => ({
  top: [b.x + b.w / 2, b.y],
  bottom: [b.x + b.w / 2, b.y + b.h],
  left: [b.x, b.y + b.h / 2],
  right: [b.x + b.w, b.y + b.h / 2],
});

// A row of boxes across the full width, joined left to right by labeled arrows.
const flow = (y, h, steps, gap = 44) => {
  const w = (W - 2 * M - gap * (steps.length - 1)) / steps.length;
  const boxes = steps.map((s, i) => ({ ...s, x: M + i * (w + gap), y, w, h }));
  return (
    boxes.map(box).join("") +
    boxes
      .slice(1)
      .map((b, i) => arrow([at(boxes[i]).right, at(b).left]))
      .join("")
  );
};

// A sequence diagram: one lifeline per actor, then steps top to bottom. A step is a message
// { from, to, label, ret (dashed reply), both (two-way) } or a note { note, over: [from, to] }.
const sequence = (actors, steps) => {
  const col = (W - 2 * M) / actors.length;
  const cx = Object.fromEntries(actors.map((a, i) => [a.id, M + col * (i + 0.5)]));
  const head = 72;
  let y = M + head + 20;
  const body = [];
  for (const s of steps) {
    if (s.note) {
      const [a, b] = s.over.map((id) => cx[id]);
      const left = Math.min(a, b) - col / 2 + 12;
      const width = Math.abs(b - a) + col - 24;
      const ls = wrap(s.note, SIZE.sub, width - 24);
      const h = ls.length * 19 + 16;
      body.push(rect("note", left, y, width, h, 8) + lines(left + width / 2, y + h / 2, ls.map((l) => [l, "sub"])));
      y += h + 22;
      continue;
    }
    const [x1, x2] = [cx[s.from], cx[s.to]];
    const ls = wrap(s.label, SIZE.label, Math.max(Math.abs(x2 - x1) - 24, col - 24));
    y += ls.length * 19;
    const mid = (x1 + x2) / 2;
    body.push(
      ls.map((l, i) => label(mid, y - 10 - (ls.length - 1 - i) * 19, l)).join("") +
        arrow([[x1, y], [x2, y]], `edge${s.ret ? " ret" : ""}${s.both ? " both" : ""}`),
    );
    y += 26;
  }
  const end = y + 4;
  const lifelines = actors
    .map((a) => `<line class="life" x1="${n(cx[a.id])}" y1="${M + head}" x2="${n(cx[a.id])}" y2="${n(end)}"/>`)
    .join("");
  const heads = actors
    .map((a) => box({ x: cx[a.id] - col / 2 + 8, y: M, w: col - 16, h: head, kind: a.kind ?? "box", text: a.text }))
    .join("");
  return { h: end + M, body: lifelines + heads + body.join("") };
};

// --- diagrams ------------------------------------------------------------------------
// Each returns { h, body, alt }.
const diagrams = {
  architecture() {
    const agent = { x: M, y: M, w: 360, h: 72, text: ["Your AI agent", "any MCP client, such as Claude Code"] };
    const human = { x: W - M - 360, y: M, w: 360, h: 72, text: ["A person", "only when a login needs them"] };
    const group = { x: M, y: 150, w: W - 2 * M, h: 196 };
    const rw = (group.w - 32 - 3 * 16) / 4;
    const role = (i, t) => ({ x: group.x + 16 + i * (rw + 16), y: group.y + 44, w: rw, h: 104, kind: "key", text: t });
    const mcp = role(0, ["mcp", "the 31 tools, over stdio; restart it any time"]);
    const listener = role(1, ["listener", "records console and network traffic, per visit"]);
    const reaper = role(2, ["reaper", "on a timer, closes sessions left idle"]);
    const takeover = role(3, ["takeover", "serves the page a person logs in through"]);
    const chrome = { x: M, y: 420, w: W - 2 * M, h: 172 };
    const sw = (chrome.w - 32 - 2 * 16) / 3;
    const sessions = ["Session a", "Session b", "Session c"].map((s, i) => ({
      x: chrome.x + 16 + i * (sw + 16),
      y: chrome.y + 64,
      w: sw,
      h: 88,
      kind: "frame",
      text: [s, "its own cookies, storage and tabs"],
    }));
    return {
      h: chrome.y + chrome.h + M,
      body:
        rect("group", group.x, group.y, group.w, group.h, 16) +
        text("title", group.x + 18, group.y + 28, "browser-session: one binary, four roles") +
        text("sub", group.x + 18, group.y + group.h - 16, "They share one state dir: sessions, visit logs, saved cookies, takeover tickets.") +
        rect("box", chrome.x, chrome.y, chrome.w, chrome.h, 14) +
        text("title", chrome.x + 18, chrome.y + 30, "Chrome, with the DevTools Protocol on :9222") +
        text("sub", chrome.x + 18, chrome.y + 50, "yours to run: headless or not, local or remote; one Chrome holds every session") +
        [agent, human, mcp, listener, reaper, takeover, ...sessions].map(box).join("") +
        // Straight down from each top box to the role it uses.
        arrow([[at(mcp).top[0], agent.y + agent.h], at(mcp).top]) +
        label(at(mcp).top[0], 126, "MCP tool calls, with a sessionId") +
        arrow([[at(takeover).top[0], human.y + human.h], at(takeover).top]) +
        label(at(takeover).top[0], 126, "opens the takeover link") +
        [mcp, listener, reaper].map((b) => arrow([at(b).bottom, [at(b).bottom[0], chrome.y]])).join("") +
        arrow([at(takeover).bottom, [at(takeover).bottom[0], chrome.y]], "edge both") +
        label(at(mcp).bottom[0], 384, "drives pages") +
        label(at(listener).bottom[0], 384, "watches every tab") +
        label(at(reaper).bottom[0], 384, "cleans up") +
        label(at(takeover).bottom[0], 384, "live view and input"),
      alt: "Your AI agent sends MCP tool calls with a sessionId to the mcp role. The browser-session binary runs as four roles that share one state dir: mcp drives pages, listener watches every tab and records console messages and requests per visit, reaper closes idle sessions on a timer, and takeover serves the page a person opens to log in, with a live view and input to Chrome. All four talk to one Chrome over the DevTools Protocol on port 9222, which holds every session as its own isolated context with its own cookies, storage and tabs",
    };
  },

  sessions() {
    const d = sequence(
      [
        { id: "agent", text: ["Your agent"] },
        { id: "mcp", kind: "key", text: ["browser-session mcp"] },
        { id: "chrome", text: ["Chrome"] },
      ],
      [
        { from: "agent", to: "mcp", label: "open_browser_session()" },
        { from: "mcp", to: "chrome", label: "create an isolated context" },
        { from: "mcp", to: "agent", label: "sessionId: \"a1b2\"", ret: true },
        { from: "agent", to: "mcp", label: "navigate(a1b2, …) and log in" },
        { note: "The MCP connection drops and the server process restarts. The context lives on in Chrome.", over: ["agent", "mcp"] },
        { from: "agent", to: "mcp", label: "take_snapshot(a1b2)" },
        { from: "mcp", to: "chrome", label: "find context a1b2 again" },
        { from: "mcp", to: "agent", label: "same tabs, still logged in", ret: true },
      ],
    );
    return {
      ...d,
      alt: "Sequence: the agent calls open_browser_session, the mcp server creates an isolated context in Chrome and returns sessionId a1b2. The agent navigates and logs in. Then the MCP connection drops and the server process restarts, but the context lives on in Chrome. The agent calls take_snapshot with a1b2, the restarted server finds the context again, and the agent gets the same tabs, still logged in",
    };
  },

  takeover() {
    const d = sequence(
      [
        { id: "agent", text: ["Your agent"] },
        { id: "mcp", kind: "key", text: ["browser-session mcp"] },
        { id: "person", text: ["A person"] },
        { id: "page", kind: "key", text: ["Takeover page"] },
        { id: "chrome", text: ["Chrome"] },
      ],
      [
        { from: "agent", to: "mcp", label: "request_human_takeover" },
        { from: "mcp", to: "agent", label: "a one-time link", ret: true },
        { from: "agent", to: "person", label: "\"please log in here\"" },
        { from: "person", to: "page", label: "opens the link" },
        { from: "page", to: "chrome", label: "screencast and input", both: true },
        { note: "They log in with their password, passkey or 2FA, straight into Chrome. The agent never sees it.", over: ["person", "chrome"] },
        { from: "person", to: "page", label: "clicks Done" },
        { from: "agent", to: "mcp", label: "await_human_takeover" },
        { from: "mcp", to: "agent", label: "completed: true", ret: true },
      ],
    );
    return {
      ...d,
      alt: "Sequence: the agent calls request_human_takeover and gets a one-time link back, which it shows to a person. The person opens the link; the takeover page streams the live Chrome tab and forwards their input. They log in with their password, passkey or 2FA straight into Chrome, and the agent never sees it. They click Done, and the agent's await_human_takeover call returns completed: true",
    };
  },

  "saved-state"() {
    return {
      h: 2 * M + 112,
      body: flow(M, 112, [
        { text: ["Session one", "log in yourself, or hand it to a person"] },
        { kind: "key", text: ["save_browser_state", "name: \"github\""] },
        { kind: "end", text: ["states/github.json", "the session's cookies, readable only by you"] },
        { kind: "key", text: ["load_browser_state", "into any new session"] },
        { text: ["Session two", "already logged in"] },
      ]),
      alt: "Session one logs in, by you or by a person through a takeover. save_browser_state with the name github writes the session's cookies to states/github.json, readable only by you. load_browser_state copies them into any new session, so session two starts already logged in",
    };
  },

};

// --- write ---------------------------------------------------------------------------
mkdirSync(path.join(root, DIR), { recursive: true });
// Only what this script writes lives in DIR (besides the palette): a diagram removed here
// disappears there too.
for (const f of readdirSync(path.join(root, DIR)))
  if (f !== "palette.json" && !Object.keys(diagrams).some((name) => f === `${name}.svg` || f === `${name}-dark.svg`))
    unlinkSync(path.join(root, DIR, f));
const built = Object.fromEntries(
  Object.entries(diagrams).map(([name, make]) => {
    const d = make();
    const [light, dark] = svg(d.h, d.body);
    write(`${DIR}/${name}.svg`, light);
    write(`${DIR}/${name}-dark.svg`, dark);
    return [name, d];
  }),
);

// Fills every marked block in a doc; unknown names fail loudly.
const fill = (file) => {
  const rel = path.relative(path.dirname(file), DIR);
  const src = read(file).replace(/<!-- diagram:([\w-]+) -->[\s\S]*?<!-- \/diagram:\1 -->/g, (_, name) => {
    if (!built[name]) throw new Error(`${file}: no diagram ${name}`);
    const alt = esc(built[name].alt).replace(/"/g, "&quot;");
    return `<!-- diagram:${name} -->\n<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="${rel}/${name}-dark.svg">\n  <img src="${rel}/${name}.svg" alt="${alt}">\n</picture>\n<!-- /diagram:${name} -->`;
  });
  write(file, src);
};
DOCS.forEach(fill);

console.log(`Wrote ${Object.keys(built).length} diagrams and filled ${DOCS.length} docs`);
