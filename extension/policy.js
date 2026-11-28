// Guest grant policy: every rule that limits the agent lives in this file.
//
// The operator and their AI are untrusted from the guest's side, and so is the
// relay server: anything it sends may be crafted. So this file decides, inside
// the guest's own Chrome, which DevTools Protocol (CDP) commands reach a tab,
// with which parameters, on which URLs, and which ones wait for the guest to
// press Allow. It has no dependencies and touches no `chrome.*` API, so the
// tests in tests/extension/ run it under plain Node, and background.js is the
// only caller.
//
// Three layers, all deny by default:
//
//   1. COMMANDS: a CDP method passes only if it has an entry below. Each entry
//      rebuilds the parameters from scratch, copying only known fields after
//      checking their type and range, so an unexpected field (a `sessionId`
//      for a child target, a `frameId`, `objectId`) never reaches Chrome.
//   2. URL scope: the agent may only be on pages that match the grant's
//      patterns, and never on a page from HARD_DENY or a private address,
//      whatever the grant says.
//   3. Approval: in "ask" mode, a mouse press, an Enter key or a Space key (both
//      activate the focused control) waits for the guest.

/** Longest a grant can last, whatever the server asks for. */
export const MAX_TTL_MS = 60 * 60 * 1000;

/** Most URL patterns one grant may carry. */
export const MAX_PATTERNS = 20;

// ---------------------------------------------------------------------------
// URL patterns
//
// A grant pattern is `scheme://host[:port]/path`, for example
//   https://github.com/settings/installations/*
//   https://*.example.com/admin/*
// - scheme is https or http, written out (no wildcard).
// - host is a hostname, optionally starting with `*.`, which matches the
//   domain itself and any subdomain. No other wildcard in the host, so
//   `https://github.com*` (which would match github.com.evil.example) is
//   refused rather than guessed at.
// - path starts with `/`; `*` matches any run of characters, slashes included.
//   It is matched against the URL's path only: the query string and fragment
//   are ignored.
// ---------------------------------------------------------------------------

const HOST_RE = /^(\*\.)?([a-z0-9-]+\.)*[a-z0-9-]+$/;

/**
 * Parse one grant pattern. Throws an Error naming the problem, so a grant with
 * a pattern this file can't read is refused as a whole.
 */
export function parsePattern(pattern) {
  if (typeof pattern !== "string" || pattern.length === 0 || pattern.length > 500) {
    throw new Error("a URL pattern must be a string of 1 to 500 characters");
  }
  const m = /^(https?):\/\/([^/:]+)(?::(\d{1,5}))?(\/.*)$/.exec(pattern);
  if (!m) {
    throw new Error(`"${pattern}" is not scheme://host/path with an http or https scheme`);
  }
  const [, scheme, rawHost, port, path] = m;
  const host = rawHost.toLowerCase();
  if (!HOST_RE.test(host)) {
    throw new Error(`"${pattern}": the host may only start with "*." and have no other wildcard`);
  }
  if (host.startsWith("*.") && host.split(".").length < 3) {
    throw new Error(`"${pattern}": "*." must be followed by a domain with a dot, like *.example.com`);
  }
  if (/[?#]/.test(path)) {
    throw new Error(`"${pattern}": patterns match the path only; drop the ? or # part`);
  }
  if (isPrivateHost(host.replace(/^\*\./, ""))) {
    throw new Error(`"${pattern}": local and private network addresses are never allowed`);
  }
  return {
    source: pattern,
    scheme,
    host: host.replace(/^\*\./, ""),
    subdomains: host.startsWith("*."),
    port: port ? Number(port) : null,
    path,
  };
}

/** Regex source for a path glob: `*` is any run of characters. */
function globToRegex(glob) {
  return glob
    .split("*")
    .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
}

/** Whether `url` (a URL object) matches one parsed pattern. */
function matchesPattern(p, url) {
  if (url.protocol !== `${p.scheme}:`) return false;
  const host = url.hostname.toLowerCase();
  if (!(host === p.host || (p.subdomains && host.endsWith(`.${p.host}`)))) return false;
  const defaultPort = p.scheme === "https" ? 443 : 80;
  const port = url.port ? Number(url.port) : defaultPort;
  if (port !== (p.port ?? defaultPort)) return false;
  return new RegExp(`^${globToRegex(p.path)}$`).test(url.pathname);
}

// ---------------------------------------------------------------------------
// Hard denylist: pages the agent is never on, whatever the grant allows.
// Account security, sign-in, keys and tokens, billing. Data, not code: add a
// row to extend it. `paths` entries match whole path segments from the start
// of the path, so "/settings/tokens" covers /settings/tokens and
// /settings/tokens/123 but not /settings/tokens-help; `*` stands for exactly
// one segment. Paths are compared lowercased and percent-decoded, so
// /Settings/%74okens is still /settings/tokens.
// ---------------------------------------------------------------------------

export const HARD_DENY = [
  {
    host: "github.com",
    why: "GitHub account security, sign-in, keys, tokens and billing",
    paths: [
      "/login",
      "/logout",
      "/session",
      "/sessions",
      "/password_reset",
      "/settings/security",
      "/settings/security-log",
      "/settings/security_analysis",
      "/settings/auth",
      "/settings/two_factor_authentication",
      "/settings/sessions",
      "/settings/tokens",
      "/settings/personal-access-tokens",
      "/settings/keys",
      "/settings/ssh",
      "/settings/gpg",
      "/settings/emails",
      "/settings/admin",
      "/settings/applications",
      "/settings/connections",
      "/settings/apps/authorizations",
      "/settings/billing",
      "/settings/payment_information",
      "/settings/copilot",
      "/organizations/*/settings/billing",
      "/organizations/*/settings/security",
      "/organizations/*/settings/secrets",
      "/organizations/*/settings/personal-access-tokens",
      "/organizations/*/settings/oauth_application_policy",
      // A repository's deploy keys and Actions secrets.
      "/*/*/settings/keys",
      "/*/*/settings/secrets",
      "/*/*/settings/security_analysis",
    ],
  },
  {
    host: "*",
    why: "passwords, two-factor, keys, tokens and payments",
    // Any path segment that is one of these, on any site.
    segments: [
      "password", "passwords", "passwd", "password-reset", "password_reset",
      "reset-password", "reset_password", "change-password", "change_password",
      "forgot-password", "forgot_password",
      "2fa", "mfa", "otp", "totp", "two-factor", "two_factor",
      "two-factor-authentication", "two_factor_authentication",
      "webauthn", "passkey", "passkeys", "security-keys", "security_keys",
      "recovery-codes", "recovery_codes",
      "security",
      "tokens", "access-tokens", "access_tokens", "api-keys", "api_keys", "apikeys",
      "keys", "ssh-keys", "ssh_keys", "gpg-keys", "gpg_keys",
      "billing", "payment", "payments", "payment-methods", "payment_methods",
      "checkout", "wallet",
    ],
  },
];

/** Lowercased, percent-decoded path segments, empty ones dropped. */
function segmentsOf(pathname) {
  let decoded = pathname;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    // A malformed escape: judge the raw path instead.
  }
  return decoded.toLowerCase().split("/").filter(Boolean);
}

function pathPrefixMatches(prefix, segs) {
  const want = prefix.toLowerCase().split("/").filter(Boolean);
  if (want.length > segs.length) return false;
  return want.every((w, i) => w === "*" || w === segs[i]);
}

/**
 * Why `url` is on the hard denylist, or null. Checks both the decoded and the
 * raw path, so neither spelling slips through.
 */
export function hardDenied(url) {
  const host = url.hostname.toLowerCase();
  const variants = [segmentsOf(url.pathname), url.pathname.toLowerCase().split("/").filter(Boolean)];
  for (const rule of HARD_DENY) {
    const hostOk = rule.host === "*" || host === rule.host || host.endsWith(`.${rule.host}`);
    if (!hostOk) continue;
    for (const segs of variants) {
      if (rule.paths?.some((p) => pathPrefixMatches(p, segs))) return rule.why;
      if (rule.segments?.some((s) => segs.includes(s))) return rule.why;
    }
  }
  return null;
}

/**
 * Local and private network hosts: the guest's own machine and LAN. The agent
 * must never use the guest's browser to reach them, so they are refused even
 * if a grant names them.
 */
export function isPrivateHost(hostname) {
  const h = hostname.toLowerCase().replace(/^\[|\]$/g, "");
  if (h === "localhost" || h.endsWith(".localhost")) return true;
  if (h.endsWith(".local") || h.endsWith(".internal") || h.endsWith(".lan") || h.endsWith(".home.arpa")) {
    return true;
  }
  const v4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    return (
      a === 0 || a === 10 || a === 127 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127) || a >= 224
    );
  }
  if (h.includes(":")) {
    // IPv6: loopback, unspecified, unique local (fc00::/7), link local (fe80::/10),
    // and IPv4-mapped addresses.
    return h === "::1" || h === "::" || /^f[cd]/.test(h) || /^fe[89ab]/.test(h) || h.startsWith("::ffff:");
  }
  return false;
}

/**
 * The scope verdict for a URL: `{ ok: true }`, or `{ ok: false, why }`. This is
 * the one check every navigation goes through: the agent's Page.navigate,
 * every redirect, link and history change the extension sees in an agent tab.
 * `about:blank` is allowed, since a new tab starts there.
 */
export function checkUrl(rawUrl, patterns) {
  if (rawUrl === "about:blank") return { ok: true };
  let url;
  try {
    url = new URL(rawUrl);
  } catch {
    return { ok: false, why: "not a valid URL" };
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return { ok: false, why: `${url.protocol} pages are never allowed` };
  }
  if (url.username || url.password) return { ok: false, why: "URLs with credentials are never allowed" };
  if (isPrivateHost(url.hostname)) {
    return { ok: false, why: "local and private network addresses are never allowed" };
  }
  const denied = hardDenied(url);
  if (denied) return { ok: false, why: `always blocked (${denied})` };
  if (!patterns.some((p) => matchesPattern(p, url))) {
    return { ok: false, why: "outside the URL patterns the guest accepted" };
  }
  return { ok: true };
}

/** What may be said about an out-of-scope URL to the agent: its origin only. */
export function originOnly(rawUrl) {
  try {
    const u = new URL(rawUrl);
    return u.protocol === "http:" || u.protocol === "https:" ? u.origin : u.protocol;
  } catch {
    return "(invalid URL)";
  }
}

// ---------------------------------------------------------------------------
// declarativeNetRequest rules: the same scope, enforced by Chrome before a
// top-level request leaves the browser, so an out-of-scope navigation in an
// agent tab (a link, a redirect, a form post) is blocked rather than noticed
// afterwards. Session rules limited to the agent's tab ids:
//   priority 1  block every main-frame request
//   priority 2  allow the grant's patterns
//   priority 3  block the hard denylist and private hosts (beats the allows)
// The webNavigation checks in background.js stay as the second layer: they
// see in-page history changes and percent-encoded paths that never make a
// request.
// ---------------------------------------------------------------------------

function escapeRe(s) {
  return s.replace(/[.+?^${}()|[\]\\]/g, "\\$&");
}

/** RE2 source (Chrome's regexFilter syntax) for one parsed grant pattern. */
export function patternToRegexFilter(p) {
  const sub = p.subdomains ? "([a-z0-9-]+\\.)*" : "";
  const defaultPort = p.scheme === "https" ? 443 : 80;
  const port = p.port && p.port !== defaultPort ? `:${p.port}` : `(:${defaultPort})?`;
  const path = p.path.split("*").map(escapeRe).join("[^?#]*");
  return `^${p.scheme}://${sub}${escapeRe(p.host)}${port}${path}([?#].*)?$`;
}

/**
 * RE2 sources for the hard denylist and private hosts, one per entry: Chrome
 * refuses a regexFilter over 2 KB compiled, which one long alternation would
 * pass.
 */
export function denyRegexFilters() {
  const tail = "([/?#].*)?$";
  const out = [
    `^https?://(localhost|[^/?#]*\\.localhost)(:\\d+)?${tail}`,
    `^https?://[^/?#]*\\.(local|internal|lan|home\\.arpa)(:\\d+)?${tail}`,
    `^https?://(0|10|127)\\.\\d+\\.\\d+\\.\\d+(:\\d+)?${tail}`,
    `^https?://(22[4-9]|2[3-5]\\d)\\.\\d+\\.\\d+\\.\\d+(:\\d+)?${tail}`,
    `^https?://(169\\.254|192\\.168)\\.\\d+\\.\\d+(:\\d+)?${tail}`,
    `^https?://172\\.(1[6-9]|2\\d|3[01])\\.\\d+\\.\\d+(:\\d+)?${tail}`,
    `^https?://100\\.(6[4-9]|[7-9]\\d|1[01]\\d|12[0-7])\\.\\d+\\.\\d+(:\\d+)?${tail}`,
    `^https?://\\[[^\\]]*\\](:\\d+)?${tail}`,
  ];
  for (const rule of HARD_DENY) {
    const host = rule.host === "*" ? "[^/?#]+" : `([a-z0-9-]+\\.)*${escapeRe(rule.host)}(:\\d+)?`;
    for (const p of rule.paths ?? []) {
      const path = p
        .split("/")
        .filter(Boolean)
        .map((s) => (s === "*" ? "[^/?#]+" : escapeRe(s)))
        .join("/");
      out.push(`^https?://${host}/${path}${tail}`);
    }
    for (const s of rule.segments ?? []) {
      out.push(`^https?://${host}(/[^?#]*)?/${escapeRe(s)}${tail}`);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// CDP command allowlist.
//
// Only what navigate, take_snapshot, take_screenshot, click and type need.
// Every allowed method acts on the one agent tab it is sent to (background.js
// attaches chrome.debugger to that tab only and never passes a child
// `sessionId`), and none of them can read cookies, storage or files, open or
// list other targets, or run script:
//
// - Page.navigate: loads a URL in the tab, after checkUrl. Only `url` is
//   copied, so no `frameId` (navigating an iframe) or `referrer`.
// - Page.captureScreenshot: pixels of this tab, which the agent sees anyway.
// - Page.getLayoutMetrics: the viewport and page size, for full-page shots.
// - Accessibility.getFullAXTree / queryAXTree: the page's accessibility tree,
//   the text and controls a screen reader announces. Chrome reports password
//   fields' values masked, and hidden inputs aren't in the tree. No `frameId`,
//   so only the top document.
// - DOM.getDocument: the root node's id only (`depth` forced to 0), which
//   querySelector needs. Never the whole tree, which would carry hidden
//   inputs' values (CSRF tokens and the like) in its attributes.
// - DOM.querySelector: finds one node's id. The selector may not test an
//   attribute's value except on a short list of harmless attributes, so it
//   can't be used to guess a hidden value one character at a time
//   (`[value^="a"]`).
// - DOM.scrollIntoViewIfNeeded, DOM.getBoxModel: where an element is, to
//   click its center. DOM.focus: focuses it, to type into it.
// - Input.dispatchMouseEvent: move, press, release with the left button, and
//   the wheel. No modifier keys, so no ctrl/shift-click opening links in
//   another window outside the agent's tab group.
// - Input.dispatchKeyEvent: one key. No Alt, and Ctrl/Meta only with A (select
//   all, to clear a field); the only editing command passed is selectAll.
// - Input.insertText: types text into the focused field.
//
// Explicitly denied, among everything else not listed: Runtime.* (evaluate,
// callFunctionOn: running script could read document.cookie and storage),
// Network.* (getCookies, getAllCookies, request and response bodies),
// Storage.*, DOMStorage.*, IndexedDB.*, CacheStorage.*, Fetch.* (intercepting
// requests), Target.* (other tabs, the browser target), Browser.* and
// Page.setDownloadBehavior (files on the guest's disk), Debugger.*, IO.*
// (reading streams and blobs), DOM.getOuterHTML / getAttributes /
// describeNode / resolveNode (hidden values, script handles),
// DOM.setFileInputFiles (uploading the guest's files), WebAuthn.*,
// Autofill.*, Emulation.*, Input.setIgnoreInputEvents (locking the guest out),
// and Page.addScriptToEvaluateOnNewDocument / setBypassCSP.
// ---------------------------------------------------------------------------

/** Families named in the docs and tests as never allowed. */
export const DENIED_EXAMPLES = [
  "Runtime.evaluate",
  "Runtime.callFunctionOn",
  "Runtime.enable",
  "Network.getCookies",
  "Network.getAllCookies",
  "Network.getResponseBody",
  "Storage.getCookies",
  "Storage.clearDataForOrigin",
  "DOMStorage.getDOMStorageItems",
  "IndexedDB.requestData",
  "Fetch.enable",
  "Target.attachToTarget",
  "Target.createTarget",
  "Target.getTargets",
  "Browser.setDownloadBehavior",
  "Browser.close",
  "Page.setDownloadBehavior",
  "Page.addScriptToEvaluateOnNewDocument",
  "Page.setBypassCSP",
  "Page.getFrameTree",
  "Page.getNavigationHistory",
  "Debugger.enable",
  "IO.read",
  "DOM.getOuterHTML",
  "DOM.getAttributes",
  "DOM.describeNode",
  "DOM.resolveNode",
  "DOM.setFileInputFiles",
  "WebAuthn.enable",
  "Autofill.trigger",
  "Emulation.setUserAgentOverride",
  "Input.setIgnoreInputEvents",
  "Input.dispatchTouchEvent",
  "Input.dispatchDragEvent",
];

class Refused extends Error {}

function obj(v) {
  return v && typeof v === "object" && !Array.isArray(v) ? v : {};
}
function num(v, name, min, max) {
  if (typeof v !== "number" || !Number.isFinite(v) || v < min || v > max) {
    throw new Refused(`${name} must be a number from ${min} to ${max}`);
  }
  return v;
}
function int(v, name, min, max) {
  if (!Number.isInteger(v)) throw new Refused(`${name} must be an integer`);
  return num(v, name, min, max);
}
function str(v, name, max) {
  if (typeof v !== "string" || v.length > max) throw new Refused(`${name} must be a string of at most ${max} characters`);
  return v;
}
function oneOf(v, name, options) {
  if (!options.includes(v)) throw new Refused(`${name} must be one of ${options.join(", ")}`);
  return v;
}
/** Copy `key` from `src` to `dst` through `check` when it is present. */
function opt(dst, src, key, check) {
  if (src[key] !== undefined) dst[key] = check(src[key]);
}
/** A node reference: exactly one of nodeId or backendNodeId. */
function nodeRef(p) {
  if (p.nodeId !== undefined) return { nodeId: int(p.nodeId, "nodeId", 1, 2 ** 31) };
  if (p.backendNodeId !== undefined) return { backendNodeId: int(p.backendNodeId, "backendNodeId", 1, 2 ** 31) };
  throw new Refused("a nodeId or backendNodeId is required");
}

/** Attributes whose value a selector may test: none of them hold secrets. */
const SELECTOR_SAFE_ATTRS = new Set([
  "id", "class", "type", "name", "role", "for", "href", "title", "alt", "placeholder",
  "aria-label", "aria-labelledby", "aria-describedby", "aria-controls", "aria-expanded",
  "aria-selected", "aria-checked", "aria-pressed", "aria-haspopup", "aria-current",
  "data-testid", "data-test-id", "data-test", "data-qa", "data-cy", "lang", "dir",
  "autocomplete", "disabled", "checked", "selected", "readonly", "required", "hidden",
]);

/**
 * Refuse a CSS selector that tests an attribute's value outside
 * SELECTOR_SAFE_ATTRS. `input[type=email]` passes; `input[value^="a"]` and
 * `[data-token*=x]` don't, since a yes/no answer per guess spells out the value.
 */
export function checkSelector(selector) {
  str(selector, "selector", 1000);
  if (selector.trim() === "") throw new Refused("selector is empty");
  // A namespace prefix ([*|value]) would hide the name from the check below.
  if (/\[\s*[^\]=]*\|(?!=)/.test(selector)) throw new Refused("namespaced attribute selectors are not allowed");
  // Attribute tests: [name op value].
  const attrs = selector.matchAll(/\[\s*([^\]\s~|^$*=]+)\s*([~|^$*]?=)?/g);
  for (const [, name, op] of attrs) {
    const n = name.toLowerCase().replace(/\\/g, "");
    if (op && !SELECTOR_SAFE_ATTRS.has(n)) {
      throw new Refused(`selectors may not test the value of the "${n}" attribute`);
    }
    if (n === "value") throw new Refused('selectors may not test the "value" attribute');
  }
  // Escapes could spell a blocked attribute name another way.
  if (/\\/.test(selector) && /\[/.test(selector)) throw new Refused("escaped characters in attribute selectors are not allowed");
  return selector;
}

const MOD_ALT = 1;
const MOD_CTRL = 2;
const MOD_META = 4;
const MOD_SHIFT = 8;

/** Whether a key event would press Enter (submit, follow a focused link). */
export function isEnterKey(p) {
  const nl = (s) => typeof s === "string" && /[\r\n]/.test(s);
  return (
    p.key === "Enter" || p.code === "Enter" || p.code === "NumpadEnter" ||
    p.windowsVirtualKeyCode === 13 || p.nativeVirtualKeyCode === 13 || nl(p.text) || nl(p.unmodifiedText)
  );
}

/** Whether a key event would press Space (activates a focused button or box). */
export function isSpaceKey(p) {
  return p.key === " " || p.code === "Space" || p.windowsVirtualKeyCode === 32 || p.text === " ";
}

/**
 * The allowlist. Each entry's `params` rebuilds the parameters from what the
 * relay sent; `approval` names what the guest is asked, if anything;
 * `navigates` marks commands whose URL goes through checkUrl first.
 */
export const COMMANDS = {
  "Page.navigate": {
    navigates: true,
    params: (p) => ({ url: str(p.url, "url", 8192) }),
  },
  "Page.captureScreenshot": {
    params: (p) => {
      const out = { fromSurface: true };
      opt(out, p, "format", (v) => oneOf(v, "format", ["png", "jpeg", "webp"]));
      opt(out, p, "quality", (v) => int(v, "quality", 0, 100));
      opt(out, p, "captureBeyondViewport", (v) => !!v);
      if (p.clip !== undefined) {
        const c = obj(p.clip);
        out.clip = {
          x: num(c.x, "clip.x", -1e6, 1e6),
          y: num(c.y, "clip.y", -1e6, 1e6),
          width: num(c.width, "clip.width", 1, 20000),
          height: num(c.height, "clip.height", 1, 50000),
          scale: c.scale === undefined ? 1 : num(c.scale, "clip.scale", 0.05, 4),
        };
      }
      return out;
    },
  },
  "Page.getLayoutMetrics": { params: () => ({}) },
  "Accessibility.getFullAXTree": {
    params: (p) => {
      const out = {};
      opt(out, p, "depth", (v) => int(v, "depth", 1, 10000));
      return out;
    },
  },
  "Accessibility.queryAXTree": {
    params: (p) => {
      const out = nodeRef(p);
      opt(out, p, "accessibleName", (v) => str(v, "accessibleName", 500));
      opt(out, p, "role", (v) => str(v, "role", 64));
      return out;
    },
  },
  "DOM.getDocument": { params: () => ({ depth: 0, pierce: false }) },
  "DOM.querySelector": {
    params: (p) => ({ nodeId: int(p.nodeId, "nodeId", 1, 2 ** 31), selector: checkSelector(p.selector) }),
  },
  "DOM.scrollIntoViewIfNeeded": { params: (p) => nodeRef(p) },
  "DOM.getBoxModel": { params: (p) => nodeRef(p) },
  "DOM.focus": { params: (p) => nodeRef(p) },
  "Input.dispatchMouseEvent": {
    params: (p) => {
      const out = {
        type: oneOf(p.type, "type", ["mouseMoved", "mousePressed", "mouseReleased", "mouseWheel"]),
        x: num(p.x, "x", -1e5, 1e5),
        y: num(p.y, "y", -1e5, 1e5),
        modifiers: 0,
      };
      opt(out, p, "button", (v) => oneOf(v, "button", ["none", "left"]));
      opt(out, p, "buttons", (v) => oneOf(v, "buttons", [0, 1]));
      opt(out, p, "clickCount", (v) => int(v, "clickCount", 0, 3));
      if (out.type === "mouseWheel") {
        out.deltaX = p.deltaX === undefined ? 0 : num(p.deltaX, "deltaX", -1e5, 1e5);
        out.deltaY = p.deltaY === undefined ? 0 : num(p.deltaY, "deltaY", -1e5, 1e5);
      }
      if (p.modifiers) throw new Refused("mouse events may not hold modifier keys");
      return out;
    },
    approval: (p) => (p.type === "mousePressed" ? "click" : null),
  },
  "Input.dispatchKeyEvent": {
    params: (p) => {
      const out = { type: oneOf(p.type, "type", ["keyDown", "keyUp", "rawKeyDown", "char"]) };
      opt(out, p, "key", (v) => str(v, "key", 32));
      opt(out, p, "code", (v) => str(v, "code", 32));
      opt(out, p, "text", (v) => str(v, "text", 4));
      opt(out, p, "unmodifiedText", (v) => str(v, "unmodifiedText", 4));
      opt(out, p, "windowsVirtualKeyCode", (v) => int(v, "windowsVirtualKeyCode", 0, 255));
      opt(out, p, "nativeVirtualKeyCode", (v) => int(v, "nativeVirtualKeyCode", 0, 65535));
      opt(out, p, "autoRepeat", (v) => !!v);
      opt(out, p, "isKeypad", (v) => !!v);
      opt(out, p, "location", (v) => int(v, "location", 0, 3));
      const mods = p.modifiers === undefined ? 0 : int(p.modifiers, "modifiers", 0, 15);
      if (mods & MOD_ALT) throw new Refused("the Alt key is not allowed");
      const selectAll = typeof out.key === "string" && out.key.toLowerCase() === "a";
      if (mods & (MOD_CTRL | MOD_META) && !selectAll) {
        throw new Refused("Ctrl and Meta are only allowed with A (select all)");
      }
      out.modifiers = mods & (MOD_CTRL | MOD_META | MOD_SHIFT);
      if (p.commands !== undefined) {
        if (!Array.isArray(p.commands) || !p.commands.every((c) => c === "selectAll")) {
          throw new Refused("the only editing command allowed is selectAll");
        }
        out.commands = p.commands.slice(0, 1);
      }
      return out;
    },
    approval: (p) => {
      if (p.type === "keyUp") return null;
      if (isEnterKey(p)) return "enter";
      if (isSpaceKey(p)) return "space";
      return null;
    },
  },
  "Input.insertText": {
    params: (p) => ({ text: str(p.text, "text", 10000) }),
    approval: (p) => (/[\r\n]/.test(p.text) ? "enter" : null),
  },
};

/**
 * Methods the extension answers itself (no CDP): read the agent tab's URL and
 * title, list the agent's tabs, or wait for the tab to finish loading. They
 * only ever report tabs in the agent's group that are in scope.
 */
export const EXTENSION_METHODS = {
  "Guest.tabInfo": { params: () => ({}) },
  "Guest.waitForLoad": {
    params: (p) => ({ timeoutMs: p.timeoutMs === undefined ? 30000 : int(p.timeoutMs, "timeoutMs", 0, 60000) }),
  },
};

/**
 * Check one command from the relay. Returns
 *   { ok: true, method, params, approval, navigates, local }
 * with sanitized params, or { ok: false, why }. Nothing in the message other
 * than `method` and `params` is used.
 */
export function checkCommand(method, rawParams) {
  if (typeof method !== "string") return { ok: false, why: "no method" };
  const local = Object.hasOwn(EXTENSION_METHODS, method) ? EXTENSION_METHODS[method] : null;
  const entry = Object.hasOwn(COMMANDS, method) ? COMMANDS[method] : null;
  if (!entry && !local) return { ok: false, why: `${method} is not allowed by the guest extension` };
  try {
    const params = (entry || local).params(obj(rawParams));
    const approval = entry?.approval ? entry.approval(params) : null;
    return { ok: true, method, params, approval, navigates: !!entry?.navigates, local: !!local };
  } catch (e) {
    if (e instanceof Refused) return { ok: false, why: `${method}: ${e.message}` };
    throw e;
  }
}

// ---------------------------------------------------------------------------
// Grants as the relay sends them, checked before the guest sees them.
// ---------------------------------------------------------------------------

/**
 * Validate a grant fetched from the relay. Returns a clean copy (patterns
 * parsed) or throws with the reason. The guest is shown exactly what this
 * returns, and only it is enforced afterwards: nothing the relay sends later
 * can widen it.
 */
export function validateGrant(raw) {
  const g = obj(raw);
  const text = (v, name, max) => {
    if (typeof v !== "string" || v.trim() === "") throw new Error(`the grant has no ${name}`);
    return v.trim().slice(0, max);
  };
  const purpose = text(g.purpose, "purpose", 200);
  const requester = text(g.requester, "requester", 200);
  if (!Array.isArray(g.allowedUrlPatterns) || g.allowedUrlPatterns.length === 0) {
    throw new Error("the grant allows no URL patterns");
  }
  if (g.allowedUrlPatterns.length > MAX_PATTERNS) throw new Error(`the grant has more than ${MAX_PATTERNS} URL patterns`);
  const patterns = g.allowedUrlPatterns.map(parsePattern);
  const startUrl = text(g.startUrl, "start URL", 8192);
  const start = checkUrl(startUrl, patterns);
  if (!start.ok) throw new Error(`the start URL is not allowed: ${start.why}`);
  const approvalMode = g.approvalMode === "auto" ? "auto" : "ask";
  const left = Number(g.expiresInMs);
  if (!Number.isFinite(left) || left <= 0) throw new Error("the grant has expired");
  return {
    purpose,
    requester,
    allowedUrlPatterns: patterns.map((p) => p.source),
    startUrl,
    approvalMode,
    // Counted from when this copy was made, capped here whatever the relay says.
    expiresInMs: Math.min(left, MAX_TTL_MS),
  };
}

/**
 * Parse a grant link: `https://<host>/guest/<32 hex token>`. Plain http only
 * for localhost (a developer's tunnel). Returns { origin, token, grantUrl,
 * socketUrl } or throws.
 */
export function parseGrantLink(link) {
  let u;
  try {
    u = new URL(String(link).trim());
  } catch {
    throw new Error("that is not a link");
  }
  const m = /^((?:\/[^/]+)*)\/guest\/([0-9a-f]{32})\/?$/.exec(u.pathname);
  if (!m) throw new Error("that is not a guest grant link (…/guest/<token>)");
  const local = u.hostname === "localhost" || u.hostname === "127.0.0.1";
  if (u.protocol !== "https:" && !(u.protocol === "http:" && local)) {
    throw new Error("grant links must use https");
  }
  const base = `${u.origin}${m[1]}`;
  const token = m[2];
  const ws = base.replace(/^http/, "ws");
  return {
    origin: u.origin,
    token,
    grantUrl: `${base}/guest/${token}/grant`,
    declineUrl: `${base}/guest/${token}/decline`,
    socketUrl: `${ws}/guest/${token}/ext`,
  };
}

/** Params as written to the guest's action log: typed text shown as a length. */
export function redactForLog(method, params) {
  const p = { ...obj(params) };
  if (method === "Input.insertText") p.text = `[${String(p.text ?? "").length} characters]`;
  if (method === "Input.dispatchKeyEvent") {
    for (const k of ["text", "unmodifiedText"]) if (p[k] !== undefined) p[k] = "[redacted]";
    if (typeof p.key === "string" && p.key.length === 1) p.key = "[character]";
    if (typeof p.code === "string" && /^Key|^Digit/.test(p.code)) p.code = "[character]";
  }
  return p;
}
