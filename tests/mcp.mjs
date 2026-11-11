// A minimal MCP stdio client for `browser-session mcp`, shared by the
// scenarios (tests/scenarios/run.mjs) and the benchmark (bench/run.mjs).
import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");

// MCP_BIN defaults to target/release/browser-session; STATE_DIR to a
// temporary directory, one per run so every connection shares it, as the
// processes of one deployment do.
export const stateDir = mkdtempSync(path.join(tmpdir(), "bs-bench-"));
export const mcpBin = process.env.MCP_BIN ?? path.join(root, "target/release/browser-session");

export async function connect() {
  const server = spawn(mcpBin, ["mcp"], {
    env: { STATE_DIR: stateDir, ...process.env },
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
  // The raw tool result: { content, structuredContent?, isError }.
  const raw = (name, args) => send("tools/call", { name, arguments: args });
  // The structured result; throws with the tool's message on an error.
  const call = async (name, args) => {
    const r = await raw(name, args);
    if (r.isError) throw new Error(r.content?.[0]?.text ?? "tool error");
    return r.structuredContent ?? {};
  };
  return { raw, call, close: () => server.kill() };
}
