#!/usr/bin/env node
// Runs bench/tasks.mjs against `browser-session mcp` over stdio and scores
// `act` the way jev-browser's bench/run.mjs scores theirs, so the numbers
// compare: a task is correct when every step ends with its expected status
// and the in-page assert holds; a task built to fail is correct when no step
// claims completion. "likely_completed" counts as done, like their
// "likely_done".
//
//   BROWSER_URL=http://localhost:9222 TYPESAFE_API_KEY=... node bench/run.mjs \
//     [--set base,hard,guard,peers,heldout1,heldout2,local|all] [--only id,id] [--concurrency 3] [--out file.json]
//
// MCP_BIN defaults to target/release/browser-session. STATE_DIR defaults to
// a temporary directory.
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import { HELDOUT1, HELDOUT2 } from "./heldout.mjs";
import { GUARD, HARD, PEERS, TASKS } from "./tasks.mjs";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const arg = (k, d) => {
  const i = process.argv.indexOf(k);
  return i > 0 ? process.argv[i + 1] : d;
};
// Extra tasks in bench/tasks.local.mjs (git-ignored), exported as LOCAL, the
// same convention as jev-browser's harness, so both can run one task file.
const localFile = path.join(path.dirname(fileURLToPath(import.meta.url)), "tasks.local.mjs");
const LOCAL = existsSync(localFile) ? (await import(localFile)).LOCAL : [];
const SETS = {
  base: TASKS,
  hard: HARD,
  guard: GUARD,
  peers: PEERS,
  heldout1: HELDOUT1,
  heldout2: HELDOUT2,
  local: LOCAL,
};
const set = arg("--set", "all");
const pool = set === "all" ? Object.values(SETS).flat() : set.split(",").flatMap((s) => SETS[s] ?? []);
const only = arg("--only")?.split(",");
const tasks = pool.filter((t) => !only || only.includes(t.id));
const concurrency = Number(arg("--concurrency", 3));
const out = arg("--out");

// ---- a minimal MCP stdio client ----------------------------------------
const server = spawn(process.env.MCP_BIN ?? path.join(root, "target/release/browser-session"), ["mcp"], {
  env: { STATE_DIR: mkdtempSync(path.join(tmpdir(), "bs-bench-")), ...process.env },
  stdio: ["pipe", "pipe", "inherit"],
});
const pending = new Map();
let nextId = 1;
readline.createInterface({ input: server.stdout }).on("line", (line) => {
  const msg = JSON.parse(line);
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
});
const send = (method, params) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    server.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
await send("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "bench", version: "0" } });
server.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
const call = async (name, args) => {
  const r = await send("tools/call", { name, arguments: args });
  if (r.isError) throw new Error(r.content?.[0]?.text ?? "tool error");
  return r.structuredContent ?? {};
};

// ---- tasks ---------------------------------------------------------------
async function check(sessionId, assert) {
  for (let i = 0; i < 3; i++) {
    try {
      return (await call("evaluate", { sessionId, expression: `(${assert})()` })).result;
    } catch (e) {
      if (i === 2) return `assert error: ${e.message.slice(0, 80)}`;
    }
  }
}

async function runTask(t) {
  const rec = { id: t.id, cat: t.cat, expect: t.expect ?? "pass", steps: [] };
  const started = Date.now();
  let sessionId;
  try {
    sessionId = (await call("open_browser_session", {})).sessionId;
    await call("navigate", { sessionId, url: t.url, timeout: 30000 });
    for (const s of t.steps) {
      const t0 = Date.now();
      let r;
      try {
        r = await call("act", {
          sessionId,
          goal: s.goal,
          values: s.values,
          maxSteps: Math.min(s.maxActions ?? 10, 32),
          confirmIrreversible: !!t.guard,
        });
      } catch (e) {
        r = { status: "error", error: e.message.slice(0, 200), steps: [] };
      }
      const step = { goal: s.goal, status: r.status, ms: Date.now() - t0, actions: r.steps.length, error: r.error };
      step.trace = r.steps.map((x) => `${x.action}${x.typed ? `(${x.typed})` : ""} ${String(x.target ?? "").slice(0, 40)} ${x.confidence.toFixed(2)} goal=${x.goal.toFixed(2)}`);
      if (r.candidates) step.candidates = r.candidates;
      if (s.assert) step.truth = await check(sessionId, s.assert);
      const want = s.expectStatus ?? "completed";
      const statusOk = r.status === want || (want === "completed" && r.status === "likely_completed");
      step.ok = statusOk && (step.truth === undefined || step.truth === true);
      rec.steps.push(step);
      if (!step.ok) break;
    }
  } catch (e) {
    rec.error = e.message.slice(0, 200);
  } finally {
    if (sessionId) await call("close_browser_session", { sessionId }).catch(() => {});
  }
  rec.ms = Date.now() - started;
  const done = (s) => s.status === "completed" || s.status === "likely_completed";
  const complete = rec.steps.length === t.steps.length;
  rec.correct =
    rec.expect === "fail" ? !rec.error && !rec.steps.some(done) : !rec.error && complete && rec.steps.every((s) => s.ok);
  rec.achieved = rec.expect === "fail" ? null : complete && rec.steps.at(-1)?.truth !== false && !String(rec.steps.at(-1)?.truth).startsWith("assert");
  rec.falseDone = rec.steps.some((s) => s.status === "completed" && s.truth !== undefined && s.truth !== true);
  const last = rec.steps.at(-1);
  console.log(
    `${rec.correct ? "✓" : "✗"} ${t.id.padEnd(22)} ${String(last?.status ?? rec.error).padEnd(18)} ` +
      `truth=${last?.truth} ${(rec.ms / 1000).toFixed(1)}s${rec.falseDone ? "  FALSE-DONE" : ""}`,
  );
  return rec;
}

const results = [];
let next = 0;
await Promise.all(
  Array.from({ length: concurrency }, async () => {
    while (next < tasks.length) results.push(await runTask(tasks[next++]));
  }),
);
server.kill();

const correct = results.filter((r) => r.correct).length;
const falseDone = results.filter((r) => r.falseDone).length;
const times = results.filter((r) => r.correct && r.expect === "pass").map((r) => r.ms).sort((a, b) => a - b);
const median = times.length ? times[Math.floor(times.length / 2)] : 0;
console.log(`\n${correct}/${results.length} correct · ${falseDone} false done · median ${(median / 1000).toFixed(1)}s per passing task`);
if (out) writeFileSync(out, JSON.stringify({ correct, total: results.length, falseDone, results }, null, 1));
