#!/usr/bin/env node
// Jev's answers for the scenarios, recorded once with a key and replayed
// without one, so CI runs the Jev scenarios with no key and no cost.
//
// The scenarios point the server at a local stand-in for the Jev API
// (JEV_BASE_URL). Recording forwards each question to Jev and saves the answer
// under a hash of the question; replaying answers from jev-answers.json and
// fails a question it has no answer for.
//
//   node tests/scenarios/jev-answers.mjs --check
//
// fails when the files that shape the questions changed since the answers were
// recorded, so a pull request that changes them records again; CI runs it
// before building anything. Record with:
//
//   TYPESAFE_API_KEY=... JEV_RECORD=1 CHROME_BIN=... MCP_BIN=... node tests/scenarios/run.mjs
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(here, "../..");
export const FILE = path.join(here, "jev-answers.json");
// What the questions are made of: what the server reads off a page, the
// pages, and the scenarios that ask.
const INPUTS = ["src/jev.rs", "tests/scenarios/run.mjs", "tests/scenarios/pages"];

export function inputsHash() {
  const h = createHash("sha256");
  const add = (rel) => {
    const p = path.join(root, rel);
    if (statSync(p).isDirectory()) {
      for (const f of readdirSync(p).sort()) add(path.posix.join(rel, f));
      return;
    }
    h.update(`${rel}\0`).update(readFileSync(p)).update("\0");
  };
  INPUTS.forEach(add);
  return h.digest("hex");
}

const load = () => (existsSync(FILE) ? JSON.parse(readFileSync(FILE, "utf8")) : { inputs: "", answers: {} });

// `normalize` takes out what changes between runs (the test server's port and
// address), so the same question hashes the same everywhere.
export async function startJevStandIn({ record, normalize = (s) => s }) {
  const saved = load();
  const used = {};
  const upstream = (process.env.JEV_UPSTREAM ?? "https://api.typesafe.ai").replace(/\/$/, "");
  const key = process.env.TYPESAFE_API_KEY;
  const server = http.createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    const id = createHash("sha256").update(normalize(`${req.url}\n${body}`)).digest("hex").slice(0, 20);
    const reply = (status, value) =>
      res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
    if (record) {
      const r = await fetch(upstream + req.url, {
        method: "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body,
      });
      const text = await r.text();
      if (!r.ok) return res.writeHead(r.status, { "content-type": "application/json" }).end(text);
      used[id] = JSON.parse(text);
      return reply(200, used[id]);
    }
    if (!(id in saved.answers)) {
      // The question itself, so a run that asks something new shows what differs.
      console.error(`no recorded Jev answer for ${id}: ${normalize(body)}`);
      return reply(400, {
        error: `no recorded Jev answer for this question (${id}); record again: see tests/scenarios/jev-answers.mjs`,
      });
    }
    used[id] = saved.answers[id];
    reply(200, used[id]);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    // A full run keeps only the answers it used; a partial one adds to the rest.
    save(partial) {
      const answers = partial ? { ...saved.answers, ...used } : used;
      const sorted = Object.fromEntries(Object.keys(answers).sort().map((k) => [k, answers[k]]));
      writeFileSync(FILE, `${JSON.stringify({ inputs: inputsHash(), answers: sorted }, null, 1)}\n`);
      return Object.keys(used).length;
    },
    close: () => server.close(),
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv.includes("--check")) {
  const { inputs } = load();
  if (inputs !== inputsHash()) {
    console.error(
      "The Jev scenarios' questions may have changed since their answers were recorded " +
        "(src/jev.rs, tests/scenarios/run.mjs or tests/scenarios/pages changed). Record them again:\n" +
        "  TYPESAFE_API_KEY=... JEV_RECORD=1 CHROME_BIN=... MCP_BIN=... node tests/scenarios/run.mjs",
    );
    process.exit(1);
  }
  console.log("Jev answers are up to date.");
}
