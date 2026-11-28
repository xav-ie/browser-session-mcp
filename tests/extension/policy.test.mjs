// Tests for the guest extension's policy (extension/policy.js): the CDP
// allowlist, the URL scope matcher and the hard denylist. No dependencies:
//   node --test 'tests/extension/*.test.mjs'
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import {
  COMMANDS,
  DENIED_EXAMPLES,
  checkCommand,
  checkSelector,
  checkUrl,
  denyRegexFilters,
  hardDenied,
  originOnly,
  parseGrantLink,
  parsePattern,
  patternToRegexFilter,
  redactForLog,
  validateGrant,
} from "../../extension/policy.js";

const github = ["https://github.com/settings/installations*", "https://github.com/apps/*"].map(parsePattern);

test("every denied example is refused, and nothing outside the allowlist passes", () => {
  for (const m of DENIED_EXAMPLES) {
    const r = checkCommand(m, {});
    assert.equal(r.ok, false, m);
    assert.match(r.why, /not allowed/, m);
    assert.ok(!Object.hasOwn(COMMANDS, m), `${m} must not be in the allowlist`);
  }
  for (const m of ["Runtime.anything", "Network.enable", "Storage.getStorageKey", "toString", "__proto__", "constructor"]) {
    assert.equal(checkCommand(m, {}).ok, false, m);
  }
  assert.equal(checkCommand(undefined, {}).ok, false);
});

test("the allowlist is exactly the documented set", () => {
  assert.deepEqual(Object.keys(COMMANDS).sort(), [
    "Accessibility.getFullAXTree",
    "Accessibility.queryAXTree",
    "DOM.focus",
    "DOM.getBoxModel",
    "DOM.getDocument",
    "DOM.querySelector",
    "DOM.scrollIntoViewIfNeeded",
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.captureScreenshot",
    "Page.getLayoutMetrics",
    "Page.navigate",
  ]);
});

test("params are rebuilt, so unknown fields never reach Chrome", () => {
  const nav = checkCommand("Page.navigate", { url: "https://github.com/apps/x", frameId: "F", referrer: "r" });
  assert.deepEqual(nav.params, { url: "https://github.com/apps/x" });
  assert.equal(nav.navigates, true);
  const doc = checkCommand("DOM.getDocument", { depth: -1, pierce: true });
  assert.deepEqual(doc.params, { depth: 0, pierce: false });
  const ax = checkCommand("Accessibility.getFullAXTree", { frameId: "F" });
  assert.deepEqual(ax.params, {});
  const shot = checkCommand("Page.captureScreenshot", { format: "png", fromSurface: false, optimizeForSpeed: true });
  assert.deepEqual(shot.params, { format: "png", fromSurface: true });
  assert.equal(checkCommand("Page.captureScreenshot", { format: "pdf" }).ok, false);
  assert.equal(checkCommand("DOM.getBoxModel", { objectId: "1" }).ok, false);
  assert.deepEqual(checkCommand("DOM.getBoxModel", { backendNodeId: 7, objectId: "1" }).params, { backendNodeId: 7 });
});

test("clicks and Enter wait for approval; moves and typing don't", () => {
  const mouse = (type, extra = {}) => checkCommand("Input.dispatchMouseEvent", { type, x: 10, y: 20, button: "left", ...extra });
  assert.equal(mouse("mousePressed").approval, "click");
  assert.equal(mouse("mouseReleased").approval, null);
  assert.equal(mouse("mouseMoved", { button: "none" }).approval, null);
  assert.equal(mouse("mousePressed", { button: "right" }).ok, false, "only the left button");
  assert.equal(mouse("mousePressed", { modifiers: 2 }).ok, false, "no ctrl-click into a new window");
  const key = (p) => checkCommand("Input.dispatchKeyEvent", p);
  assert.equal(key({ type: "keyDown", key: "Enter" }).approval, "enter");
  assert.equal(key({ type: "rawKeyDown", windowsVirtualKeyCode: 13 }).approval, "enter");
  assert.equal(key({ type: "char", text: "\r" }).approval, "enter");
  assert.equal(key({ type: "keyDown", code: "NumpadEnter" }).approval, "enter");
  assert.equal(key({ type: "keyUp", key: "Enter" }).approval, null);
  assert.equal(key({ type: "keyDown", key: " ", code: "Space" }).approval, "space");
  assert.equal(key({ type: "keyDown", key: "Tab" }).approval, null);
  assert.equal(key({ type: "keyDown", key: "w", modifiers: 2 }).ok, false, "no browser shortcuts");
  assert.equal(key({ type: "keyDown", key: "F4", modifiers: 1 }).ok, false, "no Alt");
  assert.equal(key({ type: "keyDown", key: "a", modifiers: 2, commands: ["selectAll"] }).ok, true);
  assert.equal(key({ type: "keyDown", key: "a", commands: ["insertNewline"] }).ok, false);
  assert.equal(checkCommand("Input.insertText", { text: "hello" }).approval, null);
  assert.equal(checkCommand("Input.insertText", { text: "a\nb" }).approval, "enter");
});

test("selectors can't be used to guess attribute values", () => {
  for (const ok of ["form button", "input[type=email]", "#login", 'a[href="/apps/x"]', "[aria-label='Install']", "button:not([disabled])"]) {
    assert.equal(checkSelector(ok), ok);
  }
  for (const bad of [
    'input[value^="a"]',
    "input[value]",
    '[name="authenticity_token"][value$=x]',
    "[data-token*=ab]",
    "form:has(input[VALUE^=a])",
    "[*|value^=a]",
    "[v\\61lue^=a]",
    "",
  ]) {
    assert.throws(() => checkSelector(bad), undefined, bad);
    assert.equal(checkCommand("DOM.querySelector", { nodeId: 1, selector: bad }).ok, false, bad);
  }
});

test("URL scope: patterns, schemes and subdomains", () => {
  const ok = (u, ps = github) => checkUrl(u, ps).ok;
  assert.ok(ok("https://github.com/settings/installations"));
  assert.ok(ok("https://github.com/settings/installations/123?x=1#y"));
  assert.ok(ok("https://github.com/apps/my-app/installations/new"));
  assert.ok(ok("about:blank"));
  assert.ok(!ok("https://github.com/settings/profile"));
  assert.ok(!ok("http://github.com/apps/x"), "scheme is part of the pattern");
  assert.ok(!ok("https://gist.github.com/apps/x"), "no subdomain without *.");
  assert.ok(!ok("https://github.com.evil.example/apps/x"));
  assert.ok(!ok("https://user:pw@github.com/apps/x"));
  assert.ok(!ok("javascript:alert(1)"));
  assert.ok(!ok("file:///etc/passwd"));
  assert.ok(!ok("chrome://settings"));
  const sub = [parsePattern("https://*.example.com/*")];
  assert.ok(ok("https://example.com/a", sub));
  assert.ok(ok("https://a.b.example.com/a", sub));
  assert.ok(!ok("https://badexample.com/a", sub));
  assert.ok(!ok("https://example.com:8443/a", sub), "other ports need the port in the pattern");
  assert.ok(ok("https://example.com:443/a", sub));
});

test("bad patterns are refused", () => {
  for (const bad of [
    "github.com/*",
    "*://github.com/*",
    "https://github.com*",
    "https://git*hub.com/*",
    "https://*.com/*",
    "https://github.com/a?b=*",
    "https://localhost/*",
    "http://192.168.1.1/*",
    "ftp://example.com/*",
    "",
  ]) {
    assert.throws(() => parsePattern(bad), undefined, bad);
  }
});

test("the hard denylist wins over any grant", () => {
  const everything = [parsePattern("https://github.com/*"), parsePattern("https://*.example.com/*")];
  for (const u of [
    "https://github.com/settings/tokens",
    "https://github.com/settings/tokens/new",
    "https://github.com/Settings/%74okens",
    "https://github.com/settings/security",
    "https://github.com/settings/keys",
    "https://github.com/settings/billing/summary",
    "https://github.com/settings/apps/authorizations",
    "https://github.com/sessions/sudo",
    "https://github.com/login",
    "https://github.com/organizations/acme/settings/billing",
    "https://github.com/acme/repo/settings/keys",
    "https://github.com/acme/repo/settings/secrets/actions",
    "https://shop.example.com/account/password",
    "https://example.com/user/2fa/setup",
    "https://example.com/billing",
    "https://example.com/api-keys",
  ]) {
    const r = checkUrl(u, everything);
    assert.equal(r.ok, false, u);
    assert.match(r.why, /always blocked/, u);
  }
  for (const u of [
    "https://github.com/settings/installations/1",
    "https://github.com/settings/apps/my-app",
    "https://github.com/settings/tokens-help",
    "https://github.com/acme/repo/settings/installations",
    "https://example.com/docs/passwordless-login",
  ]) {
    assert.equal(hardDenied(new URL(u)), null, u);
    assert.ok(checkUrl(u, everything).ok, u);
  }
});

test("private network addresses are never reachable", () => {
  const anything = [parsePattern("https://example.com/*")];
  for (const u of [
    "http://localhost:9222/json",
    "http://127.0.0.1/",
    "http://10.0.0.1/",
    "http://192.168.1.1/",
    "http://172.20.0.1/",
    "http://169.254.169.254/latest/meta-data",
    "http://[::1]/",
    "http://printer.local/",
  ]) {
    assert.equal(checkUrl(u, anything).ok, false, u);
  }
});

test("declarativeNetRequest regexes agree with the matcher", () => {
  const p = parsePattern("https://github.com/settings/installations*");
  const re = new RegExp(patternToRegexFilter(p), "i");
  assert.ok(re.test("https://github.com/settings/installations/1?x"));
  assert.ok(re.test("https://github.com:443/settings/installations"));
  assert.ok(!re.test("https://github.com/settings/profile"));
  assert.ok(!re.test("https://github.com.evil.example/settings/installations"));
  const sub = new RegExp(patternToRegexFilter(parsePattern("https://*.example.com/a/*")), "i");
  assert.ok(sub.test("https://x.example.com/a/b") && sub.test("https://example.com/a/"));
  assert.ok(!sub.test("https://badexample.com/a/b"));
  const deny = denyRegexFilters().map((s) => new RegExp(s, "i"));
  const blocked = (u) => deny.some((r) => r.test(u));
  assert.ok(blocked("https://github.com/settings/tokens"));
  assert.ok(blocked("https://github.com/acme/repo/settings/keys/new"));
  assert.ok(blocked("https://shop.example.com/a/password?next=/"));
  assert.ok(blocked("http://192.168.0.10:8080/admin"));
  assert.ok(blocked("http://localhost/"));
  assert.ok(!blocked("https://github.com/settings/installations/1"));
  assert.ok(!blocked("https://github.com/settings/tokens-help"));
  for (const s of denyRegexFilters()) assert.ok(s.length < 300, `short enough for Chrome: ${s}`);
});

test("grants are validated before the guest sees them", () => {
  const base = {
    purpose: "Install the deploy bot",
    requester: "Ana (ops)",
    allowedUrlPatterns: ["https://github.com/apps/*"],
    startUrl: "https://github.com/apps/deploy-bot/installations/new",
    approvalMode: "auto",
    expiresInMs: 10 * 60 * 60 * 1000,
  };
  const g = validateGrant(base);
  assert.equal(g.approvalMode, "auto");
  assert.equal(g.expiresInMs, 60 * 60 * 1000, "capped at an hour whatever the relay says");
  assert.equal(validateGrant({ ...base, approvalMode: "whatever" }).approvalMode, "ask");
  assert.throws(() => validateGrant({ ...base, startUrl: "https://github.com/settings/tokens" }));
  assert.throws(() => validateGrant({ ...base, startUrl: "https://example.com/" }));
  assert.throws(() => validateGrant({ ...base, allowedUrlPatterns: [] }));
  assert.throws(() => validateGrant({ ...base, allowedUrlPatterns: ["https://github.com*"] }));
  assert.throws(() => validateGrant({ ...base, expiresInMs: 0 }));
  assert.throws(() => validateGrant({ ...base, purpose: " " }));
});

test("grant links", () => {
  const t = "0123456789abcdef0123456789abcdef";
  const l = parseGrantLink(`https://takeover.example.com/guest/${t}`);
  assert.equal(l.socketUrl, `wss://takeover.example.com/guest/${t}/ext`);
  assert.equal(l.grantUrl, `https://takeover.example.com/guest/${t}/grant`);
  assert.equal(parseGrantLink(`http://localhost:9223/guest/${t}`).socketUrl, `ws://localhost:9223/guest/${t}/ext`);
  assert.throws(() => parseGrantLink(`http://takeover.example.com/guest/${t}`), /https/);
  assert.throws(() => parseGrantLink(`https://takeover.example.com/takeover/${t}`));
  assert.throws(() => parseGrantLink("not a link"));
});

test("the log never holds typed text, and blocked URLs are reported by origin", () => {
  assert.deepEqual(redactForLog("Input.insertText", { text: "hunter2" }), { text: "[7 characters]" });
  const k = redactForLog("Input.dispatchKeyEvent", { type: "keyDown", key: "x", code: "KeyX", text: "x" });
  assert.deepEqual(k, { type: "keyDown", key: "[character]", code: "[character]", text: "[redacted]" });
  assert.equal(redactForLog("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter" }).key, "Enter");
  assert.equal(originOnly("https://app.example.com/callback?code=secret"), "https://app.example.com");
});

test("the manifest asks for no host access, content scripts or web-accessible files", async () => {
  const m = JSON.parse(await readFile(new URL("../../extension/manifest.json", import.meta.url), "utf8"));
  assert.equal(m.manifest_version, 3);
  assert.deepEqual([...m.permissions].sort(), [
    "debugger",
    "declarativeNetRequest",
    "sidePanel",
    "storage",
    "tabGroups",
    "tabs",
    "webNavigation",
  ]);
  for (const k of [
    "host_permissions",
    "optional_host_permissions",
    "content_scripts",
    "web_accessible_resources",
    "externally_connectable",
  ]) {
    assert.ok(!(k in m), `${k} must not be in the manifest`);
  }
});
