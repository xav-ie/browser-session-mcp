// Takeover client: speaks CDP over a WebSocket to the takeover daemon, which
// relays it to the tab (gated on this link's claim, filtered to the session).
// Renders the screencast, forwards input, manages tabs, and relays vault
// autofill. The daemon also serves this page and the /claim and /done endpoints.

// Token comes from the URL (/takeover/<token>) — no server-side templating.
const TOKEN =
  (location.pathname.match(/\/takeover\/([0-9a-f]{32})/) || [])[1] || "";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const ctx = canvas.getContext("2d") as CanvasRenderingContext2D;
const controlBtn = document.getElementById("control") as HTMLButtonElement;
const endBtn = document.getElementById("end") as HTMLButtonElement;
const tabsEl = document.getElementById("tabs") as HTMLElement;
const wrapEl = document.getElementById("wrap") as HTMLElement;
const overlayEl = document.getElementById("overlay") as HTMLElement;
const urlEl = document.getElementById("url") as HTMLInputElement;
const toastEl = document.getElementById("toast") as HTMLElement;

let nextId = 1;
let lastMeta: any = null; // most recent screencastFrame metadata (for coord mapping)
let ws: WebSocket | null = null;
let wsBase: string | null = null; // this daemon's relay, e.g. wss://<host>/takeover/<token>
let ctxId: string | null = null; // this session's browserContextId — tabs filtered to it
let currentTarget: string | null = null; // targetId currently screencast
let getTargetsId: number | null = null;
let createTargetId: number | null = null;
let urlQueryId: number | null = null;
let navHistoryId: number | null = null; // pending Page.getNavigationHistory id
let refreshTimer: any = null;
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
  plus: svg(14, '<path d="M12 5v14M5 12h14"/>'),
  close: svg(13, '<path d="M18 6 6 18M6 6l12 12"/>'),
  enter: svg(
    14,
    '<path d="M9 10l-4 4 4 4"/><path d="M5 14h11a4 4 0 0 0 4-4V6"/>',
  ),
  key: svg(
    16,
    '<circle cx="7.5" cy="15.5" r="4.5"/><path d="M10.7 12.3 21 2M16 7l3 3M19 4l2 2"/>',
  ),
  plus16: svg(16, '<path d="M12 5v14M5 12h14"/>'),
};

// Whether this person has control. While false they watch: the relay refuses
// anything but these (src/takeover_relay.rs WATCH_ALLOWED), so they aren't sent.
let control = true;
let ended = false;
const WATCH_OK = new Set([
  "Emulation.setDeviceMetricsOverride",
  "Page.enable",
  "Page.getNavigationHistory",
  "Page.screencastFrameAck",
  "Page.startScreencast",
  "Page.stopScreencast",
  "Target.getTargets",
  "Target.setDiscoverTargets",
]);
const watchOk = (method: string, params?: any) =>
  WATCH_OK.has(method) ||
  (method === "Runtime.evaluate" && params?.expression === "location.href");

// Input that means the person tried to use the page while watching: a click
// or tap, a key, or a scroll (wheel, trackpad or touch drag) or a paste.
const TRIED = new Set(["mousePressed", "mouseWheel", "keyDown"]);
let lastHint = 0;
function watchHint() {
  // A scroll sends many events; say it once, not once per tick.
  if (Date.now() - lastHint < 2000) return;
  lastHint = Date.now();
  setStatus("watching: press Take control to use the page");
}

function send(method: string, params?: any): number | null {
  if (!ws || ws.readyState !== WebSocket.OPEN) return null;
  if (!control && !watchOk(method, params)) {
    if (TRIED.has(params?.type) || method === "Input.insertText") watchHint();
    return null;
  }
  const id = nextId++;
  ws.send(JSON.stringify({ id, method, params: params || {} }));
  return id;
}
// Status goes to a toast at the bottom: progress ("claiming session…") and
// errors stay until the next status, info (relay results) fades after a few
// seconds, and reaching "connected" clears it.
type StatusKind = "pending" | "ok" | "err" | "info";
let toastTimer: any = null;
function setStatus(s: string, kind: StatusKind = "info") {
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = null;
  toastEl.hidden = kind === "ok";
  if (kind === "ok") return;
  toastEl.textContent = s;
  toastEl.dataset.kind = kind;
  if (kind === "info")
    toastTimer = setTimeout(() => (toastEl.hidden = true), 3000);
}

// Every tab renders at the session viewport the agent chose (from the claim),
// never at this window's size: resizing would reflow the page out from under
// the agent and break its coordinates. Re-applying the same size is a no-op for
// tabs already at it and pulls fresh tabs/popups in line. The canvas is scaled
// locally to fit this window instead (fitCanvas).
let sessionVp = { width: 1280, height: 800 };
function applyViewport() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const { width, height } = sessionVp;
  send("Emulation.setDeviceMetricsOverride", {
    width,
    height,
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
    maxWidth: width,
    maxHeight: height,
  });
}

// --- local view: fit + pinch zoom ------------------------------------------
// The canvas is fit ("contain") into #wrap, then a local transform zooms/pans
// it (two-finger pinch). Purely client-side — the remote page never sees it,
// and toPage() maps through getBoundingClientRect, which includes the transform.
const view = { zoom: 1, x: 0, y: 0 };
const MAX_ZOOM = 5;
function fitCanvas() {
  // Fit inside #wrap's padding, which leaves room for the canvas outline
  // (drawn outside the box, so #wrap's overflow clip would cut it off).
  const cs = getComputedStyle(wrapEl);
  const w =
    wrapEl.clientWidth -
    parseFloat(cs.paddingLeft) -
    parseFloat(cs.paddingRight);
  const h =
    wrapEl.clientHeight -
    parseFloat(cs.paddingTop) -
    parseFloat(cs.paddingBottom);
  if (w <= 0 || h <= 0) return;
  // Scale up as well as down: a big window should get a big view, not a
  // 1280x800 island in the middle.
  const fit = Math.min(w / canvas.width, h / canvas.height);
  canvas.style.width = canvas.width * fit + "px";
  canvas.style.height = canvas.height * fit + "px";
  setView(view.zoom, view.x, view.y);
}
// Keep the zoomed canvas covering #wrap (or centered on an axis where it's
// smaller), so you can't pan the page off into the void.
function clampAxis(t: number, off: number, scaled: number, wrapSize: number) {
  if (scaled <= wrapSize) return (wrapSize - scaled) / 2 - off;
  return Math.min(-off, Math.max(wrapSize - scaled - off, t));
}
function setView(zoom: number, x: number, y: number) {
  view.zoom = Math.min(MAX_ZOOM, Math.max(1, zoom));
  const w = canvas.offsetWidth * view.zoom;
  const h = canvas.offsetHeight * view.zoom;
  view.x = clampAxis(x, canvas.offsetLeft, w, wrapEl.clientWidth);
  view.y = clampAxis(y, canvas.offsetTop, h, wrapEl.clientHeight);
  canvas.style.transform =
    view.zoom === 1
      ? ""
      : `translate(${view.x}px, ${view.y}px) scale(${view.zoom})`;
}

// --- vault relay ----------------------------------------------------------
// Values only ever reach the page as typed input (Input.insertText and key
// events). Scripts run in the page (loginPage) find, focus and locate fields
// and buttons; they never carry a value.

type VaultKind = "user" | "pass" | "otp";
const VAULT_IDS: Record<VaultKind, string> = {
  user: "vu",
  pass: "vp",
  otp: "vo",
};
const KIND_NAME: Record<VaultKind, string> = {
  user: "username",
  pass: "password",
  otp: "one-time code",
};
const vaultValue = (kind: VaultKind) =>
  (document.getElementById(VAULT_IDS[kind]) as HTMLInputElement).value;

// Runs inside the remote page (stringified into Runtime.evaluate), so it must
// stay self-contained: no imports, no outside names. `op` is one of:
//   reset         forget the fields filled by an earlier run
//   scan          the login step showing: each kind's field as "empty",
//                 "full" or "locked" (disabled or read-only) or absent; the
//                 submit button's state ("ready", "busy", "captcha" or
//                 "none"); any error the page shows; and a signature that
//                 changes when the page moves on (new address, different
//                 fields, or the filled fields gone)
//   fill <kind>   focus, select and remember that field; true if it can take text
//   submit <kind> the button that submits that field's step: its center
//                 { x, y } in viewport pixels once it's ready, else
//                 { wait: "busy" | "captcha" }, or null if there is none
//   stop          whether the focused element is worth stopping a Tab on: a
//                 text field, a submit button, or a frame we can't see into
function loginPage(op: string, kind?: string): unknown {
  const OTP_RE =
    /one.?time|otp|totp|2fa|mfa|two.?factor|verification|security.?code|auth\w*.?code|passcode|^code$/i;
  const USER_RE =
    /user|e-?mail|login|account|identifier|phone|online.?id|^id$/i;
  const SUBMIT_RE =
    /^\s*(log\s*-?\s*(in|on)|sign\s*-?\s*(in|on)|continue|next|submit|verify|confirm|done|ok|go)\b/i;
  const NOT_SUBMIT_RE =
    /\bwith\b|google|apple|facebook|microsoft|github|passkey|sso|sign\s*-?\s*up|register|create|forgot|reset|cancel|back/i;
  const BUSY_TEXT_RE =
    /^\s*(loading|please wait|signing|logging|verifying|checking|submitting)/i;
  const SPINNER_RE =
    /^(spinner|loader|loading|spin|animate-spin|[\w-]*-spinner|spinner-[\w-]*)$/i;
  const BUSY_CLASS_RE = /^(is-)?(loading|busy|disabled|submitting)$/i;
  const CAPTCHA =
    "[name=cf-turnstile-response], [name=g-recaptcha-response], [name=h-captcha-response]";
  const TEXT_TYPES = ["", "text", "email", "tel", "number", "password", "url"];
  const MARK = Symbol.for("browser-session.autofill");
  const w = window as any;
  if (op === "reset") {
    w[MARK] = [];
    return true;
  }
  const marks: WeakRef<Element>[] = (w[MARK] ||= []);

  // Elements in document order, through open shadow roots.
  const all = (sel: string): Element[] => {
    const out: Element[] = [];
    const walk = (root: Document | ShadowRoot) => {
      root.querySelectorAll("*").forEach((el) => {
        if (el.matches(sel)) out.push(el);
        if (el.shadowRoot) walk(el.shadowRoot);
      });
    };
    walk(document);
    return out;
  };
  const visible = (el: Element) => {
    const r = el.getBoundingClientRect();
    const s = getComputedStyle(el);
    return (
      r.width > 2 &&
      r.height > 2 &&
      s.visibility !== "hidden" &&
      s.display !== "none" &&
      Number(s.opacity) > 0.05
    );
  };
  const classes = (el: Element) =>
    (el.getAttribute("class") || "").split(/\s+/).filter(Boolean);
  const hint = (el: Element) => {
    const i = el as HTMLInputElement;
    const labels = Array.from(i.labels || [])
      .map((l) => l.textContent)
      .join(" ");
    return [
      i.name,
      i.id,
      i.placeholder,
      el.getAttribute("aria-label"),
      i.autocomplete,
      labels,
    ]
      .join(" ")
      .trim();
  };
  const textInput = (el: Element) =>
    el.tagName === "TEXTAREA" ||
    (el.tagName === "INPUT" &&
      TEXT_TYPES.includes((el.getAttribute("type") || "").toLowerCase()));
  const locked = (el: Element) =>
    (el as HTMLInputElement).disabled || (el as HTMLInputElement).readOnly;
  const textField = (el: Element) => textInput(el) && !locked(el);
  const label = (el: Element) =>
    ((el as HTMLInputElement).value && el.tagName === "INPUT"
      ? (el as HTMLInputElement).value
      : el.textContent || el.getAttribute("aria-label") || ""
    ).trim();
  // A button that submits, whether or not it's usable yet.
  const isSubmit = (el: Element) => {
    if (!visible(el)) return false;
    const text = label(el);
    if (NOT_SUBMIT_RE.test(text)) return false;
    const type = (el.getAttribute("type") || "").toLowerCase();
    const formSubmit =
      (el.tagName === "BUTTON" && (type === "" || type === "submit")) ||
      (el.tagName === "INPUT" && (type === "submit" || type === "image"));
    return (
      (formSubmit && !!(el as HTMLButtonElement).form) ||
      SUBMIT_RE.test(text) ||
      BUSY_TEXT_RE.test(text)
    );
  };
  // Disabled, marked busy, showing a spinner or a "Signing in…" label: a
  // click now would be lost.
  const busy = (b: Element) =>
    (b as HTMLButtonElement).disabled ||
    b.getAttribute("aria-disabled") === "true" ||
    !!b.closest("[aria-busy=true]") ||
    getComputedStyle(b).pointerEvents === "none" ||
    classes(b).some((c) => BUSY_CLASS_RE.test(c)) ||
    BUSY_TEXT_RE.test(label(b)) ||
    Array.from(b.querySelectorAll("*")).some(
      (d) =>
        (d.getAttribute("role") === "progressbar" ||
          classes(d).some((c) => SPINNER_RE.test(c))) &&
        visible(d),
    );

  const fields = all("input, textarea").filter(
    (el) => textInput(el) && visible(el),
  );
  const isOtp = (el: Element) =>
    (el as HTMLInputElement).autocomplete === "one-time-code" ||
    OTP_RE.test(hint(el)) ||
    // Split code boxes: one digit each.
    (el as HTMLInputElement).maxLength === 1;
  const find = (k: string | null | undefined) => {
    const pass = fields.find(
      (el) => (el as HTMLInputElement).type === "password" && !isOtp(el),
    ) as HTMLInputElement | undefined;
    if (k === "pass") return pass;
    if (k === "otp")
      return fields.find(
        (el) => isOtp(el) && el.getAttribute("type") !== "password",
      ) as HTMLInputElement | undefined;
    if (k !== "user") return undefined;
    const users = fields.filter(
      (el) =>
        (el as HTMLInputElement).type !== "password" &&
        el.tagName === "INPUT" &&
        !isOtp(el) &&
        !/search/i.test(hint(el)),
    ) as HTMLInputElement[];
    const named = users.find(
      (el) =>
        /username|email/.test(el.autocomplete) ||
        el.type === "email" ||
        USER_RE.test(hint(el)),
    );
    if (named) return named;
    // Otherwise the text field just before the password one.
    if (pass) {
      const before = users.filter(
        (el) =>
          el.compareDocumentPosition(pass) & Node.DOCUMENT_POSITION_FOLLOWING,
      );
      return before[before.length - 1];
    }
    return undefined;
  };

  const submitButton = (anchor: HTMLInputElement | undefined) => {
    const form = anchor?.form;
    const inForm = form
      ? all("button, input[type=submit], input[type=image]").filter(
          (el) => (el as HTMLButtonElement).form === form && isSubmit(el),
        )
      : [];
    return (
      inForm.find(
        (el) => SUBMIT_RE.test(label(el)) || BUSY_TEXT_RE.test(label(el)),
      ) ||
      inForm[0] ||
      all("button, [role=button], input[type=button], input[type=submit]").find(
        (el) =>
          isSubmit(el) &&
          (SUBMIT_RE.test(label(el)) || BUSY_TEXT_RE.test(label(el))),
      )
    );
  };
  // A captcha whose answer isn't in yet, in the form or on the page.
  const captchaPending = (anchor: HTMLInputElement | undefined) =>
    Array.from((anchor?.form || document).querySelectorAll(CAPTCHA)).some(
      (el) => !(el as HTMLInputElement).value,
    );
  const buttonState = (anchor: HTMLInputElement | undefined) => {
    const btn = submitButton(anchor);
    if (!btn) return "none";
    if (busy(btn)) return "busy";
    if (captchaPending(anchor)) return "captcha";
    return "ready";
  };
  const errorText = () => {
    const texts: string[] = [];
    for (const el of all(
      "[role=alert], [aria-live=assertive], [aria-invalid=true]",
    )) {
      let t = "";
      if (el.getAttribute("aria-invalid") === "true") {
        const ids = (el.getAttribute("aria-describedby") || "").split(/\s+/);
        t = ids
          .map((id) => (id && document.getElementById(id)?.textContent) || "")
          .join(" ");
        if (!t.trim()) t = "a field was marked invalid";
      } else if (visible(el)) t = el.textContent || "";
      t = t.replace(/\s+/g, " ").trim();
      if (t && t.length < 200 && !BUSY_TEXT_RE.test(t)) texts.push(t);
    }
    return texts.join(" · ").slice(0, 160);
  };
  const state = (el: HTMLInputElement | undefined) =>
    !el ? undefined : locked(el) ? "locked" : el.value ? "full" : "empty";

  if (op === "scan") {
    const f = { user: find("user"), pass: find("pass"), otp: find("otp") };
    const anchor = f.otp || f.pass || f.user;
    const alive = marks.filter((r) => {
      const e = r.deref();
      return e && e.isConnected && visible(e);
    }).length;
    return {
      user: state(f.user),
      pass: state(f.pass),
      otp: state(f.otp),
      button: buttonState(anchor),
      error: errorText(),
      sig: [location.href, !!f.user, !!f.pass, !!f.otp, alive].join("|"),
    };
  }

  if (op === "fill") {
    const el = find(kind);
    if (!el || locked(el)) return false;
    el.scrollIntoView({ block: "center" });
    el.focus();
    try {
      el.select();
    } catch {
      // Not every input type supports a selection.
    }
    if (!marks.some((r) => r.deref() === el)) marks.push(new WeakRef(el));
    return true;
  }

  if (op === "submit") {
    const anchor = find(kind);
    const btn = submitButton(anchor);
    if (!btn) return null;
    if (busy(btn)) return { wait: "busy" };
    if (captchaPending(anchor)) return { wait: "captcha" };
    btn.scrollIntoView({ block: "center" });
    const r = btn.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }

  if (op === "stop") {
    let a = document.activeElement;
    while (a && a.shadowRoot && a.shadowRoot.activeElement)
      a = a.shadowRoot.activeElement;
    if (!a || a === document.body) return false;
    if (a.tagName === "IFRAME" || a.tagName === "SELECT") return true;
    return textField(a) || (isSubmit(a) && !busy(a));
  }
  return undefined;
}

// Run loginPage in the tab and return its value; undefined when the page is
// mid-navigation, the script threw, or no answer came within 3 s.
async function inPage(op: string, kind?: string): Promise<any> {
  const expression = `(${loginPage.toString()})(${JSON.stringify(op)}, ${JSON.stringify(kind ?? null)})`;
  const r = await Promise.race([
    call("Runtime.evaluate", { expression, returnByValue: true }),
    new Promise((resolve) => setTimeout(resolve, 3000)),
  ]);
  const res = r as any;
  if (!res || res.exceptionDetails) return undefined;
  return res.result?.value;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

type Key = { key: string; code: string; keyCode: number; text?: string };
const TAB: Key = { key: "Tab", code: "Tab", keyCode: 9 };
const ENTER: Key = { key: "Enter", code: "Enter", keyCode: 13, text: "\r" };
// A key press the way Chrome's own input does it: rawKeyDown for keys that
// type nothing, keyDown with text (which also fires keypress) for those that do.
function press(k: Key, modifiers = 0) {
  const base = {
    key: k.key,
    code: k.code,
    windowsVirtualKeyCode: k.keyCode,
    nativeVirtualKeyCode: k.keyCode,
    modifiers,
  };
  send("Input.dispatchKeyEvent", {
    type: k.text ? "keyDown" : "rawKeyDown",
    text: k.text,
    unmodifiedText: k.text,
    ...base,
  });
  send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
}

function clickAt(x: number, y: number) {
  const base = { x, y, button: "left", modifiers: 0 };
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mouseMoved",
    button: "none",
  });
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mousePressed",
    buttons: 1,
    clickCount: 1,
  });
  send("Input.dispatchMouseEvent", {
    ...base,
    type: "mouseReleased",
    buttons: 0,
    clickCount: 1,
  });
}

// Type into the focused field, replacing what's selected. A code goes in one
// key at a time, so split one-digit boxes advance to the next box themselves.
function typeValue(kind: VaultKind, text: string) {
  if (kind !== "otp") {
    send("Input.insertText", { text });
    return;
  }
  for (const ch of text) {
    if (/\d/.test(ch))
      press({
        key: ch,
        code: "Digit" + ch,
        keyCode: 48 + Number(ch),
        text: ch,
      });
    else send("Input.insertText", { text: ch });
  }
}

// After a value, move on the way a person would, but past the links in
// between (show password, forgot username, remember me): Tab up to MAX_SKIP
// times until a text field or a submit button has focus. If none does, go back
// to where the first Tab landed.
const MAX_SKIP = 5;
async function advance(): Promise<boolean> {
  for (let i = 1; i <= MAX_SKIP; i++) {
    press(TAB);
    if (await inPage("stop")) return true;
  }
  for (let i = 1; i < MAX_SKIP; i++) press(TAB, 8);
  return false;
}

async function relay(id: string) {
  const kind = (Object.keys(VAULT_IDS) as VaultKind[]).find(
    (k) => VAULT_IDS[k] === id,
  )!;
  const text = vaultValue(kind);
  if (!text) {
    setStatus("nothing to send: autofill the box first, then try again");
    return;
  }
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    setStatus("not connected", "err");
    return;
  }
  const a = { key: "a", code: "KeyA", keyCode: 65 };
  press(a, 2); // select what's there, so the value replaces it
  typeValue(kind, text);
  const found = await advance();
  setStatus(
    found
      ? `${KIND_NAME[kind]} sent, on to the next field`
      : `${KIND_NAME[kind]} sent; no field or button within ${MAX_SKIP} tabs`,
  );
}

// Autofill works from what the page shows, not a fixed script, so it handles
// one-page logins, an email step before the password, a step that asks for
// the email again with the password, and a one-time code step after:
//   1. wait for a login step: fields showing that are empty and have a value
//      in the vault (a code typed into the vault box late is picked up too)
//   2. fill them
//   3. wait until the step's button is usable (not disabled, busy or behind
//      an unanswered captcha), then click it, or press Enter if there is none
//   4. watch what happens: the page moves on (go to 1), shows an error (stop
//      and say it), or stays put (click again, up to MAX_TRIES)
// A field kind is filled at most MAX_FILLS times, so a page that keeps
// clearing a rejected password doesn't loop.
const autoBtn = document.getElementById("autologin") as HTMLButtonElement;
let autoRun = 0; // bumped to cancel a run in progress
const KINDS: VaultKind[] = ["user", "pass", "otp"];
const MAX_TRIES = 3;
const MAX_FILLS = 3;
const NEXT_STEP_MS = 90000; // time to pick a code method, get the code, etc.
const READY_MS = 60000; // a busy button or a captcha the person solves
const OUTCOME_MS = 20000;
const NO_FIELD_MS = 15000;

type Scan = {
  user?: string;
  pass?: string;
  otp?: string;
  button: "ready" | "busy" | "captcha" | "none";
  error: string;
  sig: string;
};

function autoIdle() {
  autoBtn.removeAttribute("aria-busy");
  autoBtn.textContent = "Autofill";
}

async function autofill() {
  if (autoBtn.getAttribute("aria-busy") === "true") {
    autoRun++;
    autoIdle();
    setStatus("autofill stopped");
    return;
  }
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    setStatus("not connected", "err");
    return;
  }
  if (!KINDS.some((k) => vaultValue(k))) {
    setStatus("nothing to fill: autofill the boxes first, then try again");
    return;
  }
  const run = ++autoRun;
  const live = () => run === autoRun && control && !ended;
  autoBtn.setAttribute("aria-busy", "true");
  autoBtn.textContent = "Stop";

  const fills: Record<VaultKind, number> = { user: 0, pass: 0, otp: 0 };
  const shown = (s: Scan | undefined) => KINDS.filter((k) => s?.[k]);
  const todo = (s: Scan | undefined) =>
    KINDS.filter(
      (k) => s?.[k] === "empty" && vaultValue(k) && fills[k] < MAX_FILLS,
    );
  const scan = (): Promise<Scan | undefined> => inPage("scan");
  // Poll the page until `ok` says so or `ms` pass; the last scan either way.
  async function until(
    ms: number,
    ok: (s: Scan | undefined, waited: number) => boolean,
  ) {
    const t0 = Date.now();
    let s: Scan | undefined;
    while (live()) {
      s = await scan();
      if (ok(s, Date.now() - t0) || Date.now() - t0 > ms) break;
      await sleep(400);
    }
    return s;
  }
  const finish = (msg: string, kind: StatusKind = "info") => {
    if (live()) setStatus(msg, kind);
  };

  let steps = 0;
  let tries = 0; // clicks on this step that didn't move the page
  try {
    await inPage("reset");
    while (live()) {
      // 1. A step to work on.
      const first = steps === 0 && tries === 0;
      if (!first && !tries)
        setStatus("waiting for the next login step…", "pending");
      const s = await until(first ? 3000 : NEXT_STEP_MS, (s, waited) => {
        if (todo(s).length || ((tries || first) && shown(s).length))
          return true;
        // No login field at all for a while: logged in, or a step that
        // needs the person (pick where to send a code).
        if (!shown(s).length && waited > NO_FIELD_MS) return true;
        // A code field with the vault box still empty: ask for the code
        // and keep waiting for it.
        if (s?.otp === "empty" && !vaultValue("otp"))
          setStatus(
            "type the one-time code into the box; autofill goes on once it's there",
            "pending",
          );
        return false;
      });
      if (!live()) return;
      const fill = todo(s);
      if (!fill.length && !((tries || first) && shown(s).length)) {
        if (first) finish("no login field found on this page", "err");
        else
          finish(
            `autofill done after ${steps} step${steps === 1 ? "" : "s"}; press it again for a later step`,
          );
        return;
      }

      // 2. Fill.
      for (const k of fill) {
        if (!(await inPage("fill", k))) continue;
        typeValue(k, vaultValue(k));
        fills[k]++;
        await sleep(200); // let the page's own input handlers run
      }
      const anchor = [...shown(s)].reverse()[0] ?? "user";
      if (fill.length)
        setStatus(
          `filled ${fill.map((k) => KIND_NAME[k]).join(" and ")}, submitting`,
          "pending",
        );

      // 3. Wait for a usable button, then click it.
      await sleep(300); // validation that runs as the field changes
      let at: any = null;
      for (const t0 = Date.now(); live() && Date.now() - t0 < READY_MS;) {
        at = await inPage("submit", anchor);
        if (at && at.x !== undefined) break;
        if (at === null && Date.now() - t0 > 2000) break; // no button: Enter
        if (Date.now() - t0 > 1500)
          setStatus(
            at?.wait === "captcha"
              ? "waiting for the captcha: solve it on the page"
              : "waiting for the page's button to be ready…",
            "pending",
          );
        await sleep(300);
      }
      if (!live()) return;
      if (at?.wait) {
        finish(
          at.wait === "captcha"
            ? "the captcha wasn't solved; solve it, then press Autofill again"
            : "the page's button stayed busy; press Autofill again when it's ready",
          "err",
        );
        return;
      }
      const before = await scan();
      if (at) clickAt(at.x, at.y);
      else {
        await inPage("fill", anchor);
        press(ENTER);
      }

      // 4. Did it go anywhere?
      let sawBusy = false;
      const after = await until(OUTCOME_MS, (s, waited) => {
        if (!s) return false; // mid-navigation
        if (s.sig !== before?.sig) return true;
        if (s.error && s.error !== before?.error) return true;
        if (s.button === "busy") sawBusy = true;
        // It went busy and came back on the same step, or never reacted.
        return (
          (sawBusy && s.button !== "busy" && waited > 1000) ||
          (!sawBusy && waited > 4000)
        );
      });
      if (!live()) return;
      if (after && after.sig !== before?.sig) {
        steps++;
        tries = 0;
        continue;
      }
      if (after?.error && after.error !== before?.error) {
        finish(`the page says: ${after.error}`, "err");
        return;
      }
      if (++tries >= MAX_TRIES) {
        finish(
          `the page didn't move on after ${MAX_TRIES} tries; check it and press Autofill again`,
          "err",
        );
        return;
      }
      setStatus("the page didn't move on, trying again…", "pending");
    }
  } catch {
    finish("autofill stopped: lost the page", "err");
  } finally {
    if (run === autoRun) autoIdle();
  }
}
autoBtn.addEventListener("click", (e) => {
  e.preventDefault();
  autofill();
});

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
  // A single tab needs no strip; the bar's + button opens more.
  tabsEl.hidden = !multi;
}

// Replies awaited by call() (one-off requests that need their result).
const pending = new Map<number, (result: any) => void>();
function call(method: string, params?: any): Promise<any> {
  const id = send(method, params);
  if (id === null) return Promise.reject(new Error("not connected"));
  return new Promise((resolve) => pending.set(id, resolve));
}

function onMessage(ev: MessageEvent) {
  let msg: any;
  try {
    msg = JSON.parse(ev.data);
  } catch {
    return;
  }

  // Added by the relay, not Chrome: a handback, retake or end, from this page
  // or another of the person's.
  if (msg.method === "Takeover.control") {
    setControl(!!msg.params?.control);
    return;
  }
  if (msg.method === "Takeover.ended") {
    showEnded();
    return;
  }

  if (msg.id && pending.has(msg.id)) {
    const resolve = pending.get(msg.id)!;
    pending.delete(msg.id);
    resolve(msg.result);
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
        fitCanvas();
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
    setStatus("connected", "ok");
    send("Page.enable");
    // NOTE: deliberately NOT Runtime.enable — it turns on console-event
    // reporting, which is the CDP tell bot-detectors catch. Runtime.evaluate
    // (used for the URL/nav) works without it.
    send("Target.setDiscoverTargets", { discover: true });
    focusTab();
    applyViewport();
    urlQueryId = send("Runtime.evaluate", {
      expression: "location.href",
      returnByValue: true,
    });
    controlBtn.disabled = false;
    endBtn.disabled = false;
    reconnects = 0;
    refreshNav();
    canvas.focus();
    refreshTabs();
  };
  const sock = ws;
  // Switching tabs or the link ending drops `ws` first, so only an unexpected
  // close (Chrome restarted, network gone, a phone pausing the page) still
  // finds this socket current.
  ws.onclose = () => {
    if (ws !== sock) return;
    ws = null;
    setStatus("connection lost, reconnecting…", "err");
    scheduleReconnect();
  };
  ws.onerror = () =>
    setStatus("connection error: is chrome.<base> reachable?", "err");
  ws.onmessage = onMessage;
}

// --- input forwarding -----------------------------------------------------
// Displayed canvas (fit + local zoom) → the page's viewport in CSS pixels. The
// frame shows the whole device screen, so map by the frame's device size, like
// DevTools' own screencast does; pageScaleFactor is already baked into the
// image, so dividing by it again misaims every click once it isn't 1.
function toPage(ev: { clientX: number; clientY: number }) {
  const rect = canvas.getBoundingClientRect();
  const w = (lastMeta && lastMeta.deviceWidth) || canvas.width;
  const h = (lastMeta && lastMeta.deviceHeight) || canvas.height;
  const offTop = (lastMeta && lastMeta.offsetTop) || 0;
  return {
    x: ((ev.clientX - rect.left) * w) / rect.width,
    y: ((ev.clientY - rect.top) * h) / rect.height - offTop,
  };
}
// Remote CSS pixels per displayed CSS pixel (for turning drags into scrolls).
function pageUnitsPerPx() {
  const w = (lastMeta && lastMeta.deviceWidth) || canvas.width;
  return w / canvas.getBoundingClientRect().width;
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

// --- touch ----------------------------------------------------------------
// Phones send no real mouse: one finger taps (→ click) or drags (→ scrolls the
// remote page); two fingers pinch-zoom/pan the local view so the full-size
// viewport is readable on a small screen. `touch-action: none` on the canvas
// keeps the browser from zooming the takeover page itself, and cancelling
// pointerdown suppresses the compatibility mouse events (no double input).
const TAP_SLOP = 10;
const touches = new Map<number, { x: number; y: number }>();
let gesture:
  | { kind: "tap" | "scroll"; sx: number; sy: number; lx: number; ly: number }
  | {
      kind: "pinch";
      dist: number;
      cx: number;
      cy: number;
      zoom: number;
      x: number;
      y: number;
    }
  | { kind: "none" }
  | null = null;
function pinchState() {
  const [a, b] = [...touches.values()];
  return {
    dist: Math.hypot(a.x - b.x, a.y - b.y) || 1,
    cx: (a.x + b.x) / 2,
    cy: (a.y + b.y) / 2,
  };
}
function tap(ev: PointerEvent) {
  const { x, y } = toPage(ev);
  clickAt(x, y);
}
canvas.addEventListener("pointerdown", (e) => {
  if (e.pointerType === "mouse") return;
  e.preventDefault();
  canvas.focus();
  canvas.setPointerCapture(e.pointerId);
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (touches.size === 1) {
    gesture = {
      kind: "tap",
      sx: e.clientX,
      sy: e.clientY,
      lx: e.clientX,
      ly: e.clientY,
    };
  } else if (touches.size === 2) {
    gesture = {
      kind: "pinch",
      ...pinchState(),
      zoom: view.zoom,
      x: view.x,
      y: view.y,
    };
  } else {
    gesture = { kind: "none" };
  }
});
canvas.addEventListener("pointermove", (e) => {
  if (e.pointerType === "mouse" || !touches.has(e.pointerId) || !gesture)
    return;
  touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (gesture.kind === "tap" || gesture.kind === "scroll") {
    if (
      gesture.kind === "tap" &&
      Math.hypot(e.clientX - gesture.sx, e.clientY - gesture.sy) > TAP_SLOP
    ) {
      gesture.kind = "scroll";
    }
    if (gesture.kind === "scroll") {
      const k = pageUnitsPerPx();
      const { x, y } = toPage(e);
      send("Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x,
        y,
        deltaX: (gesture.lx - e.clientX) * k,
        deltaY: (gesture.ly - e.clientY) * k,
      });
    }
    gesture.lx = e.clientX;
    gesture.ly = e.clientY;
  } else if (gesture.kind === "pinch") {
    // Zoom about the fingers' midpoint: the content point that was under the
    // starting midpoint stays under the current one.
    const p = pinchState();
    const zoom = Math.min(
      MAX_ZOOM,
      Math.max(1, (gesture.zoom * p.dist) / gesture.dist),
    );
    const r = wrapEl.getBoundingClientRect();
    const ox = r.left + canvas.offsetLeft;
    const oy = r.top + canvas.offsetTop;
    const ux = (gesture.cx - ox - gesture.x) / gesture.zoom;
    const uy = (gesture.cy - oy - gesture.y) / gesture.zoom;
    setView(zoom, p.cx - ox - ux * zoom, p.cy - oy - uy * zoom);
  }
});
function touchEnd(e: PointerEvent) {
  if (e.pointerType === "mouse" || !touches.has(e.pointerId)) return;
  touches.delete(e.pointerId);
  if (e.type === "pointerup" && gesture && gesture.kind === "tap") tap(e);
  // Lifting one finger of a pinch must not turn into a stray tap or scroll.
  gesture = touches.size ? { kind: "none" } : null;
}
canvas.addEventListener("pointerup", touchEnd);
canvas.addEventListener("pointercancel", touchEnd);

// Keys that type something without being one character: Enter must carry
// "\r", or Chrome fires no keypress and forms never submit.
const KEY_TEXT: Record<string, string> = { Enter: "\r" };
function keyEvent(type: string, ev: KeyboardEvent) {
  const text =
    type === "keyDown"
      ? ev.key.length === 1
        ? ev.key
        : KEY_TEXT[ev.key]
      : undefined;
  send("Input.dispatchKeyEvent", {
    type: type === "keyDown" && !text ? "rawKeyDown" : type,
    key: ev.key,
    code: ev.code,
    windowsVirtualKeyCode: ev.keyCode,
    nativeVirtualKeyCode: ev.keyCode,
    text,
    unmodifiedText: text,
    modifiers: modBits(ev),
  });
}
// ⌘V / Ctrl+V pastes THIS machine's clipboard: forwarded as keys it would
// paste the remote browser's (empty) clipboard, or type a stray "v" since Linux
// Chrome ignores Cmd. So the shortcut stays local, the browser fires `paste`,
// and the text is typed into the remote page's focused field.
const isPasteKey = (e: KeyboardEvent) =>
  (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "v";
let pasteFallback: any = null;
function pasteText(text: string) {
  if (!text) return;
  send("Input.insertText", { text });
  setStatus("pasted " + text.length + " characters");
}
// ⌘C / Ctrl+C copies the remote page's selection to THIS machine's clipboard
// (text selected in an input/textarea too; never a password field's).
const isCopyKey = (e: KeyboardEvent) =>
  (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "c";
const SELECTION_JS = `(() => {
  const a = document.activeElement;
  if (a && (a.tagName === "TEXTAREA" || (a.tagName === "INPUT" && a.type !== "password"))
      && typeof a.selectionStart === "number")
    return a.value.slice(a.selectionStart, a.selectionEnd);
  return String(getSelection());
})()`;
function copySelection() {
  const text = call("Runtime.evaluate", {
    expression: SELECTION_JS,
    returnByValue: true,
  }).then((r) => String((r && r.result && r.result.value) || ""));
  // A promise-valued ClipboardItem is written inside this key press, which
  // Safari requires; the remote answer arrives a moment later.
  const write =
    typeof ClipboardItem !== "undefined"
      ? navigator.clipboard.write([
          new ClipboardItem({
            "text/plain": text.then((t) => {
              if (!t) throw new Error("empty");
              return new Blob([t], { type: "text/plain" });
            }),
          }),
        ])
      : text.then((t) => {
          if (!t) throw new Error("empty");
          return navigator.clipboard.writeText(t);
        });
  Promise.all([text, write])
    .then(([t]) => setStatus("copied " + t.length + " characters"))
    .catch(() =>
      text.then(
        (t) =>
          setStatus(
            t ? "couldn't write the clipboard" : "nothing selected",
            t ? "err" : "info",
          ),
        () => setStatus("not connected", "err"),
      ),
    );
}

canvas.addEventListener("keydown", (e) => {
  if (isCopyKey(e)) {
    e.preventDefault();
    copySelection();
    return;
  }
  if (isPasteKey(e)) {
    // Safari fires no `paste` on a non-editable element; read the clipboard
    // directly there (it shows its own Paste prompt).
    pasteFallback = setTimeout(() => {
      navigator.clipboard
        ?.readText()
        .then(pasteText)
        .catch(() => setStatus("clipboard not readable here", "err"));
    }, 50);
    return;
  }
  e.preventDefault();
  keyEvent("keyDown", e);
});
canvas.addEventListener("keyup", (e) => {
  if (isPasteKey(e) || isCopyKey(e)) return;
  e.preventDefault();
  keyEvent("keyUp", e);
});
document.addEventListener("paste", (e) => {
  if (document.activeElement !== canvas) return; // URL bar, vault: normal paste
  e.preventDefault();
  clearTimeout(pasteFallback);
  pasteText(e.clipboardData?.getData("text/plain") || "");
});

// Re-fit the local canvas whenever the screencast area changes size: window
// resizes, and the tab strip appearing or disappearing. The remote viewport
// stays at the session size.
new ResizeObserver(fitCanvas).observe(wrapEl);

// --- address bar + back/forward ------------------------------------------
function navTo() {
  let u = urlEl.value.trim();
  if (!u) return;
  if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(u)) u = "https://" + u;
  send("Page.navigate", { url: u });
  urlEl.blur();
}
const backBtn = document.getElementById("back") as HTMLButtonElement;
const fwdBtn = document.getElementById("fwd") as HTMLButtonElement;
backBtn.innerHTML = ICONS.back;
fwdBtn.innerHTML = ICONS.fwd;
(document.getElementById("keys") as HTMLElement).innerHTML = ICONS.key;
const newTabBtn = document.getElementById("newtab") as HTMLElement;
newTabBtn.innerHTML = ICONS.plus16;
newTabBtn.addEventListener("click", () => {
  createTargetId = send("Target.createTarget", {
    url: "about:blank",
    browserContextId: ctxId,
  });
});
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

urlEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    navTo();
  }
});

// --- control, watch and end ---------------------------------------------
// The person starts in control. Hand back lets the agent drive while they
// watch; Take control pauses the agent again. The daemon records each, and
// the relay tells every open page (Takeover.control).
function focusTab() {
  if (!control) return;
  send("Page.bringToFront");
  if (currentTarget) send("Target.activateTarget", { targetId: currentTarget });
}

function setControl(c: boolean) {
  const changed = c !== control;
  control = c;
  document.body.classList.toggle("watching", !c);
  controlBtn.textContent = c ? "Hand back" : "Take control";
  controlBtn.title = c
    ? "Let the agent continue. You keep watching and can take control again."
    : "Pause the agent and use the page yourself.";
  urlEl.readOnly = !c;
  if (!c) hidePopovers();
  if (changed && c) {
    focusTab();
    canvas.focus();
  }
  if (changed)
    setStatus(
      c
        ? "you have control; the agent is paused"
        : "watching: the agent is driving",
    );
}

function hidePopovers() {
  document.querySelectorAll<HTMLElement>("[popover]").forEach((p) => {
    try {
      p.hidePopover();
    } catch {}
  });
}

async function post(action: string): Promise<boolean> {
  try {
    const r = await fetch("/takeover/" + TOKEN + "/" + action, {
      method: "POST",
    });
    return r.ok;
  } catch {
    return false;
  }
}

controlBtn.addEventListener("click", async () => {
  const want = !control;
  controlBtn.disabled = true;
  setStatus(want ? "taking control…" : "handing back to the agent…", "pending");
  if (await post(want ? "control" : "release")) setControl(want);
  else setStatus("that didn't go through, try again", "err");
  controlBtn.disabled = false;
});

endBtn.addEventListener("click", async () => {
  if (
    !confirm(
      "End this link? The agent keeps control and you can't watch or take control again.",
    )
  )
    return;
  endBtn.disabled = true;
  if (await post("done")) showEnded();
  else {
    setStatus("failed to end the link, try again", "err");
    endBtn.disabled = false;
  }
});

function showEnded(why?: string) {
  if (ended) return;
  ended = true;
  if (ws) {
    try {
      send("Page.stopScreencast");
    } catch {}
    const sock = ws;
    ws = null; // an intentional close, not "connection lost"
    sock.close();
  }
  controlBtn.disabled = true;
  endBtn.disabled = true;
  hidePopovers();
  setStatus("ended", "ok");
  overlayEl.textContent =
    why ||
    "This link has ended and the agent has control. You can close this tab.";
  overlayEl.style.display = "flex";
}

// A dropped socket comes back on its own: phones pause background pages, and
// the daemon keeps the link for a minute after its last page goes away.
let reconnectTimer: any = null;
let reconnects = 0; // since the last socket that opened
function scheduleReconnect() {
  if (reconnectTimer || ended) return;
  if (++reconnects > 5) {
    setStatus("connection lost, reload to reconnect", "err");
    return;
  }
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    if (document.visibilityState === "visible") start();
  }, 2000);
}
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible" && !ws && !ended && wsBase)
    start();
});

(document.getElementById("vaultclose") as HTMLElement).innerHTML = ICONS.close;
// Enter in a vault box runs Autofill, as Enter in a login form logs in.
document.querySelectorAll("#vault input").forEach((i) =>
  i.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key !== "Enter") return;
    e.preventDefault();
    autofill();
  }),
);
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
  setStatus("claiming session…", "pending");
  let r: Response;
  try {
    r = await fetch("/takeover/" + TOKEN + "/claim", { method: "POST" });
  } catch {
    setStatus("network error reaching the takeover server", "err");
    return;
  }
  if (r.status === 409) {
    setStatus("This takeover link is already in use by someone else.", "err");
    return;
  }
  if (r.status === 410) {
    const why = await r.text().catch(() => "");
    showEnded(
      /session .* closed/.test(why)
        ? "The browser session this link was for has closed. You can close this tab."
        : undefined,
    );
    return;
  }
  if (!r.ok) {
    setStatus("could not claim this link (HTTP " + r.status + ")", "err");
    return;
  }
  let info: any;
  try {
    info = await r.json();
  } catch {
    setStatus("bad claim response", "err");
    return;
  }
  if (!info.wsBase || !info.targetId) {
    setStatus("no session URL returned", "err");
    return;
  }
  // The relay base is a path on this daemon; same host, ws(s) scheme.
  const base = new URL(info.wsBase, location.href);
  base.protocol = base.protocol === "https:" ? "wss:" : "ws:";
  wsBase = base.href.replace(/\/$/, "");
  ctxId = info.ctxId;
  if (info.viewport && info.viewport.width > 0 && info.viewport.height > 0)
    sessionVp = info.viewport;
  canvas.width = sessionVp.width;
  canvas.height = sessionVp.height;
  fitCanvas();
  setControl(info.control !== false);
  // A reconnect stays on the tab being shown.
  openTab(currentTarget || info.targetId);
}
start();
