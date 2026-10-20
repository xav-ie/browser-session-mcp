#!/usr/bin/env node
// Stages a real human takeover and screenshots what the person sees, for the README:
// docs/images/takeover.png. It starts its own pieces on ports the OS picks, so it never touches
// a stack that's already running: a Chrome for the agent's session, the takeover daemon, the
// MCP server, and a second Chrome for the person, who opens the takeover link.
// Run after `nix build` (it uses ./result/bin/browser-session, which bundles the takeover UI):
//   node scripts/screenshot.mjs [url]        url defaults to GitHub's sign-in page
// CHROME picks the Chrome binary (default: google-chrome).
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const BIN = path.join(root, "result/bin/browser-session");
const CHROME = process.env.CHROME ?? "google-chrome";
const OUT = path.join(root, "docs/images/takeover.png");
const url = process.argv[2] ?? "https://github.com/login";
// The person's window; the session's viewport leaves room for the takeover bar above it.
const SCREEN = { width: 1280, height: 800 };
const VIEWPORT = { width: 1280, height: 756 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, check) => {
  for (let i = 0; i < 80; i++) {
    const v = await check().catch(() => undefined);
    if (v) return v;
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}`);
};

if (!existsSync(BIN)) throw new Error(`${BIN} is missing: run nix build first`);
const tmp = mkdtempSync(path.join(tmpdir(), "browser-session-screenshot-"));
const children = [];
const start = (cmd, args, env = {}) => {
  const child = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: ["pipe", "pipe", "ignore"] });
  children.push(child);
  return child;
};

// A Chrome on a port of its own choosing, which it writes to DevToolsActivePort.
const chrome = async (name, flags = []) => {
  const dir = path.join(tmp, name);
  start(CHROME, ["--headless=new", "--remote-debugging-port=0", "--remote-allow-origins=*", `--user-data-dir=${dir}`, ...flags]);
  const port = await until(`${name} Chrome`, async () => readFileSync(path.join(dir, "DevToolsActivePort"), "utf8").split("\n")[0]);
  return `127.0.0.1:${port}`;
};
const freePort = () =>
  new Promise((r) => {
    const s = createServer().listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => r(port));
    });
  });

try {
  const session = await chrome("session", ["--disable-blink-features=AutomationControlled"]);
  const person = await chrome("person", ["--hide-scrollbars"]);
  const takeover = `127.0.0.1:${await freePort()}`;
  const env = {
    STATE_DIR: path.join(tmp, "state"),
    BROWSER_URL: `http://${session}`,
    CHROME_WS_BASE: `ws://${session}`,
    TAKEOVER_BIND: takeover,
    TAKEOVER_BASE_URL: `http://${takeover}`,
  };
  start(BIN, ["takeover"], env);
  await until("the takeover daemon", () => fetch(`http://${takeover}/`).then(() => true));

  // --- the agent: MCP over stdio -------------------------------------------------
  const mcp = start(BIN, ["mcp"], env);
  const replies = new Map();
  let id = 0;
  readline.createInterface({ input: mcp.stdout }).on("line", (line) => {
    const m = JSON.parse(line);
    replies.get(m.id)?.(m);
  });
  const rpc = (method, params) =>
    new Promise((r) => {
      replies.set(++id, r);
      mcp.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  const tool = async (name, args) => {
    const m = await rpc("tools/call", { name, arguments: args });
    if (m.error || m.result.isError) throw new Error(`${name}: ${JSON.stringify(m.error ?? m.result.content)}`);
    return m.result.structuredContent;
  };
  await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "screenshot", version: "0" } });
  mcp.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
  const { sessionId } = await tool("open_browser_session", { viewport: VIEWPORT });
  await tool("navigate", { sessionId, url });
  await sleep(1500);
  const { url: link } = await tool("request_human_takeover", { sessionId });

  // --- the person: opens the link in their own browser -----------------------------
  const { webSocketDebuggerUrl } = await (await fetch(`http://${person}/json/version`)).json();
  const ws = new WebSocket(webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener("open", r));
  const results = new Map();
  let cid = 0;
  ws.addEventListener("message", (e) => {
    const m = JSON.parse(e.data);
    results.get(m.id)?.(m);
  });
  const cdp = (method, params = {}, target) =>
    new Promise((r, j) => {
      results.set(++cid, (m) => (m.error ? j(new Error(`${method}: ${m.error.message}`)) : r(m.result)));
      ws.send(JSON.stringify({ id: cid, method, params, sessionId: target }));
    });
  const { targetId } = await cdp("Target.createTarget", { url: "about:blank" });
  const { sessionId: tab } = await cdp("Target.attachToTarget", { targetId, flatten: true });
  await cdp("Emulation.setDeviceMetricsOverride", { ...SCREEN, deviceScaleFactor: 2, mobile: false }, tab);
  await cdp("Page.navigate", { url: link }, tab);
  // Long enough for the claim, the screencast's first frames, and the page's fonts.
  await sleep(6000);
  const { data } = await cdp("Page.captureScreenshot", { format: "png" }, tab);
  writeFileSync(OUT, Buffer.from(data, "base64"));
  ws.close();
  console.log(`Wrote ${path.relative(root, OUT)} from ${url}`);
} finally {
  for (const child of children) child.kill();
  await sleep(500);
  rmSync(tmp, { recursive: true, force: true });
}
