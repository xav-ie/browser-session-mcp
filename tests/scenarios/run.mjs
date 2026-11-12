#!/usr/bin/env node
// Scenarios: tool-level checks on local pages (pages/) that reproduce what
// agents ran into in real use: a popup or cookie banner swallowing clicks, a
// modal fading out, a focus trap, hidden and far-down targets, content that
// loads late, screenshots stuck in a sandbox, phone checks, logins lost
// between tasks, web component buttons, opaque script errors. The pages are
// served from a local HTTP server, so the runs need no network and don't
// change under us.
//
//   BROWSER_URL=http://localhost:9222 | CHROME_BIN=/path/to/chrome \
//     [TYPESAFE_API_KEY=...] [TAKEOVER_BASE_URL=...] node tests/scenarios/run.mjs [--only id,id]
//
// With CHROME_BIN instead of BROWSER_URL it starts its own headless Chrome.
// Scenarios that need Jev or takeover are skipped without them. MCP_BIN and
// STATE_DIR: see tests/mcp.mjs.
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { networkInterfaces } from "node:os";
import { connect, mcpBin, stateDir } from "../mcp.mjs";

const pages = path.join(path.dirname(fileURLToPath(import.meta.url)), "pages");
const server = http.createServer((req, res) => {
  const name = path.basename(new URL(req.url, "http://x").pathname) || "home.html";
  // An API answering with a token and a cookie, for the network log.
  if (name === "items") {
    res.writeHead(200, { "content-type": "application/json", "set-cookie": "sid=s3cr3t; Path=/" });
    res.end('{"items":[{"id":1},{"id":2}],"accessToken":"tok-456"}');
    return;
  }
  if (name === "upload") {
    req.resume();
    req.on("end", () => res.writeHead(204).end());
    return;
  }
  // An API that takes a while, for pages that fill themselves in.
  if (name === "slow.json") {
    setTimeout(() => res.writeHead(200, { "content-type": "application/json" }).end('{"count":12}'), 800);
    return;
  }
  try {
    const body = readFileSync(path.join(pages, name.endsWith(".html") ? name : `${name}.html`));
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" }).end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
// Every interface: the listener blocks loopback (so a page can't probe local
// ports), and its scenario reaches the pages by the machine's own address.
await new Promise((r) => server.listen(0, "0.0.0.0", r));
const SITE = `http://127.0.0.1:${server.address().port}`;
const LAN = Object.values(networkInterfaces())
  .flat()
  .find((i) => i.family === "IPv4" && !i.internal)?.address;
const LAN_SITE = LAN && `http://${LAN}:${server.address().port}`;

// A Chrome of our own, when given one to start.
let chrome;
if (!process.env.BROWSER_URL && process.env.CHROME_BIN) {
  const probe = http.createServer();
  await new Promise((r) => probe.listen(0, "127.0.0.1", r));
  const port = probe.address().port;
  probe.close();
  // The flags scripts/smoke.sh starts Chrome with (it runs as root in CI).
  chrome = spawn(process.env.CHROME_BIN, [
    "--headless", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage",
    `--remote-debugging-port=${port}`, `--user-data-dir=${mkdtempSync(path.join(tmpdir(), "bs-scenarios-"))}`,
  ], { stdio: ["ignore", "ignore", "pipe"] });
  let log = "";
  chrome.stderr.on("data", (d) => (log = (log + d).slice(-2000)));
  process.env.BROWSER_URL = `http://127.0.0.1:${port}`;
  for (let i = 0; ; i++) {
    const up = await fetch(`${process.env.BROWSER_URL}/json/version`).then((r) => r.ok, () => false);
    if (up) break;
    if (i > 100) throw new Error(`Chrome didn't come up:\n${log}`);
    await new Promise((r) => setTimeout(r, 200));
  }
}
if (!process.env.BROWSER_URL) throw new Error("set BROWSER_URL, or CHROME_BIN to start a Chrome");

const PHONE_UA =
  "Mozilla/5.0 (Linux; Android 14; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";

// A PNG's pixel size, from its IHDR chunk.
const pngSize = (file) => {
  const b = readFileSync(file);
  if (b.toString("latin1", 1, 4) !== "PNG") throw new Error(`${file} is not a PNG`);
  return { width: b.readUInt32BE(16), height: b.readUInt32BE(20) };
};

class Failed extends Error {}
class Skipped extends Error {}
const expect = (cond, what) => {
  if (!cond) throw new Failed(what);
};
const text = (r) => r.content?.[0]?.text ?? "";
const js = async (call, sessionId, expression) => (await call("evaluate", { sessionId, expression })).result;

const SCENARIOS = [
  {
    id: "popup-covers-click",
    about: "A newsletter popup covers a swatch: the click fails naming the popup's host, and lands once it's closed",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/product.html` });
      await call("wait_for", { sessionId, expression: "document.getElementById('popup-root').shadowRoot", timeout: 5000 });
      const blocked = await raw("click", { sessionId, selector: 'label[for="color-steel-grey"]' });
      expect(blocked.isError, "the covered click fails");
      expect(text(blocked).includes("(inside div#popup-root)"), `the error names the popup's host: ${text(blocked).slice(0, 200)}`);
      expect((await js(call, sessionId, "document.getElementById('color-steel-grey').checked")) === false, "the swatch wasn't picked");
      // CSS reaches into the shadow root for the close button.
      await call("click", { sessionId, selector: "button.close" });
      // It fades out; until it's gone it still covers the page.
      await call("wait_for", { sessionId, expression: "!document.getElementById('popup-root').shadowRoot.querySelector('.veil')", timeout: 3000 });
      const r = await call("click", { sessionId, selector: 'label[for="color-steel-grey"]' });
      expect(r.hit?.startsWith("label"), `the click landed on the label: ${r.hit}`);
      expect(!r.dialog, `the closed cart drawer off screen isn't an open dialog: ${r.dialog}`);
      expect((await js(call, sessionId, "document.getElementById('color-steel-grey').checked")) === true, "the swatch is picked");
    },
  },
  {
    id: "cookie-banner",
    about: "A cookie banner over a button: the click fails naming it, and lands once it's accepted",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/product.html` });
      await call("wait_for", { sessionId, expression: "document.getElementById('popup-root').shadowRoot", timeout: 5000 });
      await call("evaluate", { sessionId, expression: "(document.getElementById('popup-root').remove(), 1)" });
      const blocked = await raw("click", { sessionId, selector: "#add" });
      expect(blocked.isError && text(blocked).includes("covered by div#cookie-banner"), `the error names the banner: ${text(blocked).slice(0, 160)}`);
      await call("click", { sessionId, selector: "#cookie-accept" });
      await call("click", { sessionId, selector: "#add" });
      expect((await js(call, sessionId, "document.title")) === "added", "the button was clicked");
    },
  },
  {
    id: "focus-trap",
    about: "press_key reports where focus went, so walking a modal's focus trap needs no evaluate",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/dialog.html` });
      const opened = await call("click", { sessionId, selector: "#open" });
      expect(opened.dialog?.includes("Height"), `the click reports the open dialog: ${opened.dialog}`);
      const seen = [];
      for (let i = 0; i < 4; i++) seen.push((await call("press_key", { sessionId, key: "Tab" })).focused);
      const back = (await call("press_key", { sessionId, key: "Shift+Tab" })).focused;
      expect(seen.join(" / ") === "button#apply \"Apply\" / button#close \"Close\" / input#height / button#apply \"Apply\"",
        `Tab cycles inside the dialog: ${seen.join(" / ")}`);
      expect(back === "input#height", `Shift+Tab goes back: ${back}`);
      const walk = await call("press_key", { sessionId, key: "Tab", repeat: 3 });
      expect(walk.focusTrail?.join(" / ") === "button#apply \"Apply\" / button#close \"Close\" / input#height",
        `repeat reports focus after each press: ${walk.focusTrail?.join(" / ")}`);
      const closed = await call("press_key", { sessionId, key: "Escape" });
      expect(!closed.dialog, `Escape closes it: ${closed.dialog}`);
    },
  },
  {
    id: "link-click-navigates",
    about: "A click that follows a link reports the new URL",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/home.html` });
      const r = await call("click", { sessionId, selector: 'a[href="/collection.html"]' });
      expect(r.navigated && r.url.endsWith("/collection.html") && r.title === "Collection", `the click reports the new page: ${JSON.stringify(r)}`);
    },
  },
  {
    id: "navigate-settles",
    about: "navigate can wait for a self-rendering page's requests to go quiet, or for its text",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/app.html` });
      const early = await js(call, sessionId, "document.getElementById('root').textContent");
      expect(early === "", `at the load event the page is still blank: ${JSON.stringify(early)}`);
      await call("navigate", { sessionId, url: `${SITE}/app.html`, waitUntil: "networkidle" });
      expect((await js(call, sessionId, "document.getElementById('root').textContent")) === "12 products", "networkidle waits for it to fill in");
      await call("navigate", { sessionId, url: `${SITE}/app.html`, text: "12 products" });
      const missing = await raw("navigate", { sessionId, url: `${SITE}/app.html`, text: "no such text", settleTimeout: 1500 });
      expect(missing.isError && text(missing).includes("didn't appear"), `text that never shows fails: ${text(missing).slice(0, 120)}`);
    },
  },
  {
    id: "hidden-and-far",
    about: "A hidden button fails instead of a click at (0, 0); one far down a smooth-scrolling page is hit",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/scroll.html` });
      const hidden = await raw("click", { sessionId, selector: "#hidden", timeout: 500 });
      expect(hidden.isError && /not visible|not found/.test(text(hidden)), `a hidden button fails: ${text(hidden).slice(0, 160)}`);
      const far = await call("click", { sessionId, selector: "#far" });
      expect(far.hit?.startsWith("button#far"), `it landed on the far button: ${far.hit}`);
      expect((await js(call, sessionId, "document.title")) === "far clicked", "the far button was clicked");
      const off = await raw("click", { sessionId, x: 640, y: -40 });
      expect(off.isError && /outside the screen/.test(text(off)), "a point off the screen fails");
    },
  },
  {
    id: "wait-for-js",
    about: "wait_for pauses without a timer, and waits on page JavaScript",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/loading.html` });
      const t0 = Date.now();
      await call("wait_for", { sessionId, ms: 500 });
      expect(Date.now() - t0 >= 450, "ms pauses about that long");
      await call("click", { sessionId, selector: "#start" });
      await call("wait_for", { sessionId, expression: "dataLayer.some(e => e.event === 'view_item')", timeout: 5000 });
      expect((await js(call, sessionId, "document.getElementById('finish').hidden")) === false, "the content showed");
    },
  },
  {
    id: "screenshot-file",
    about: "A screenshot of one element saves to a file on the host, at the asked scale",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/home.html` });
      const r = await call("take_screenshot", { sessionId, selector: "h1", scale: 0.5, save: true });
      expect(r.path?.endsWith(".png"), `it returns the file's path: ${JSON.stringify(r)}`);
      // The file outlives the session: an agent often reads it after closing.
      await call("close_browser_session", { sessionId });
      const other = await session();
      await call("navigate", { sessionId: other, url: `${SITE}/home.html` });
      const box = await js(call, other, "(() => { const r = document.querySelector('h1').getBoundingClientRect(); return [r.width, r.height]; })()");
      const { width, height } = pngSize(r.path);
      expect(Math.abs(width - box[0] / 2) <= 2 && Math.abs(height - box[1] / 2) <= 2,
        `the file is the heading at half size: ${width}x${height} for a ${box.map(Math.round).join("x")} heading`);
    },
  },
  {
    id: "phone-device",
    about: "A specific phone: its User-Agent, pixel ratio and touch screen, and screenshots at its resolution",
    async run({ call, session }) {
      const sessionId = await session({ useMobileUA: true, userAgent: PHONE_UA, viewport: { width: 360, height: 780, deviceScaleFactor: 3 } });
      await call("navigate", { sessionId, url: `${SITE}/product.html` });
      const seen = await js(call, sessionId, "({ ua: navigator.userAgent, dpr: devicePixelRatio, coarse: matchMedia('(pointer: coarse)').matches, w: innerWidth })");
      expect(seen.ua === PHONE_UA, `the page sees the phone's User-Agent: ${seen.ua}`);
      expect(seen.dpr === 3, `the pixel ratio is 3: ${seen.dpr}`);
      expect(seen.coarse, "the page sees a touch screen");
      expect(seen.w === 360, `the page is 360 wide: ${seen.w}`);
      const shot = await call("take_screenshot", { sessionId, save: true });
      expect(pngSize(shot.path).width === 1080, `the screenshot is 1080 device pixels wide: ${pngSize(shot.path).width}`);
    },
  },
  {
    id: "device-presets",
    about: "A device preset sets the screen, pixel ratio, User-Agent and touch at once",
    async run({ call, raw, session }) {
      const iphone = await session({ device: "iphone-15" });
      await call("navigate", { sessionId: iphone, url: `${SITE}/home.html` });
      const a = await js(call, iphone, "({ ua: navigator.userAgent, dpr: devicePixelRatio, w: screen.width, coarse: matchMedia('(pointer: coarse)').matches })");
      expect(a.ua.includes("iPhone") && a.dpr === 3 && a.w === 393 && a.coarse, `an iPhone 15: ${JSON.stringify(a)}`);
      const pixel = await session({ device: "pixel-7" });
      await call("navigate", { sessionId: pixel, url: `${SITE}/home.html` });
      const b = await js(call, pixel, "navigator.userAgentData.getHighEntropyValues(['model']).then(h => ({ ua: navigator.userAgent, model: h.model, dpr: devicePixelRatio, w: screen.width }))");
      expect(b.ua.includes("Pixel 7") && /Chrome\/\d+\./.test(b.ua) && b.model === "Pixel 7" && b.dpr === 2.625 && b.w === 412,
        `a Pixel 7, its client hints included: ${JSON.stringify(b)}`);
      const unknown = await raw("open_browser_session", { device: "nokia-3310" });
      expect(unknown.isError && text(unknown).includes("iphone-15"), `an unknown device lists the presets: ${text(unknown).slice(0, 120)}`);
    },
  },
  {
    id: "named-session",
    about: "A named session is reused while open, and its login survives closing it",
    async run({ call, opened }) {
      const name = `scenario-${process.pid}`;
      try {
        const a = await call("open_browser_session", { name });
        opened.push(a.sessionId);
        const again = await call("open_browser_session", { name });
        expect(again.sessionId === a.sessionId, "opening the name again reuses the session");
        await call("navigate", { sessionId: a.sessionId, url: `${SITE}/home.html` });
        await call("evaluate", { sessionId: a.sessionId, expression: "(document.cookie = 'login=yes; max-age=3600; path=/', 1)" });
        await call("close_browser_session", { sessionId: a.sessionId });
        const b = await call("open_browser_session", { name });
        opened.push(b.sessionId);
        expect(b.sessionId !== a.sessionId, "a closed name opens a new session");
        await call("navigate", { sessionId: b.sessionId, url: `${SITE}/home.html` });
        expect((await js(call, b.sessionId, "document.cookie")).includes("login=yes"), "the new session has the login cookie");
      } finally {
        await call("delete_browser_state", { name }).catch(() => {});
      }
    },
  },
  {
    id: "one-call-at-a-time",
    about: "Two agents on one session take turns instead of fighting over the page",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/home.html` });
      // A second agent: its own server process, as another connection is.
      const other = await connect();
      try {
        const t0 = Date.now();
        const first = call("wait_for", { sessionId, ms: 2000 }).catch((e) => e);
        await new Promise((r) => setTimeout(r, 300));
        await other.call("evaluate", { sessionId, expression: "1" });
        const waited = Date.now() - t0;
        await first;
        expect(waited >= 1900, `the second call waited for the first: ${waited}ms`);
      } finally {
        other.close();
      }
    },
  },
  {
    id: "page-dialogs",
    about: "The page's own dialogs are answered and reported, so \"Leave site?\" can't hang a tab",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/editor.html` });
      const del = await call("click", { sessionId, selector: "#delete" });
      expect(del.answered?.some((d) => d.includes("Delete 3 products?")), `the confirm is reported: ${JSON.stringify(del.answered)}`);
      expect((await js(call, sessionId, "document.title")) === "deleted", "the confirm was accepted");
      await call("type", { sessionId, selector: "#title", text: "Draft" });
      const left = await call("navigate", { sessionId, url: `${SITE}/home.html`, timeout: 5000 });
      expect(left.url.endsWith("/home.html"), `leaving an edited page goes through: ${left.url}`);
    },
  },
  {
    id: "no-passkey-prompt",
    about: "A passkey request fails at once, as if cancelled, so the page offers a password instead of stalling",
    async run({ call, session }) {
      const sessionId = await session();
      // Passkeys need a site name, not an IP address.
      await call("navigate", { sessionId, url: `${SITE.replace("127.0.0.1", "localhost")}/signin.html` });
      await call("click", { sessionId, selector: "#passkey" });
      await call("wait_for", { sessionId, expression: "document.title === 'use a password'", timeout: 3000 });
    },
  },
  {
    id: "network-details",
    about: "The network log has headers and response bodies on request, credentials masked, and large request bodies",
    async run({ call, session }) {
      if (!LAN_SITE) throw new Skipped("no non-loopback address to serve the page on");
      const listener = spawn(mcpBin, ["listener"], {
        env: { ...process.env, STATE_DIR: stateDir },
        stdio: "ignore",
      });
      try {
        await new Promise((r) => setTimeout(r, 1500));
        const sessionId = await session();
        await call("navigate", { sessionId, url: `${LAN_SITE}/api.html`, text: "2 items" });
        await call("wait_for", { sessionId, ms: 1500 });
        const plain = await call("list_network_requests", { sessionId, urlContains: "/api/items" });
        expect(plain.requests.length === 1 && !plain.requests[0].requestHeaders, `without details, no headers: ${JSON.stringify(plain.requests).slice(0, 200)}`);
        const { requests } = await call("list_network_requests", { sessionId, urlContains: "/api/", details: true });
        const items = requests.find((r) => r.url.endsWith("/api/items"));
        expect(items, `the API call is logged: ${requests.map((r) => r.url).join(", ")}`);
        const h = (o, k) => Object.entries(o ?? {}).find(([n]) => n.toLowerCase() === k)?.[1];
        expect(h(items.requestHeaders, "authorization") === "[redacted]", `the bearer token is masked: ${JSON.stringify(items.requestHeaders)}`);
        expect(h(items.requestHeaders, "content-type") === "application/json", "other request headers are kept");
        expect(h(items.responseHeaders, "content-type") === "application/json", `response headers are kept: ${JSON.stringify(items.responseHeaders)}`);
        // Chrome leaves Set-Cookie out of response headers; masked if it ever shows.
        expect(!JSON.stringify(items).includes("s3cr3t"), "the session cookie isn't in the log");
        expect(items.post_data === '{"q":"hoodie"}', `the request body: ${items.post_data}`);
        expect(items.responseBody?.includes('"items":[{"id":1},{"id":2}]') && !items.responseBody.includes("tok-456"),
          `the response body, token masked: ${items.responseBody}`);
        const upload = requests.find((r) => r.url.endsWith("/api/upload"));
        expect(upload?.post_data?.length === 100000, `a large request body is kept whole: ${upload?.post_data?.length}`);
      } finally {
        listener.kill();
      }
    },
  },
  {
    id: "measure-page",
    about: "measure_page reports paints, layout shift and blocking time, rated, from the page's own timing",
    async run({ call, session }) {
      const sessionId = await session();
      const m = await call("measure_page", { sessionId, url: `${SITE}/slow.html` });
      expect(m.longTasks >= 1 && m.longestTaskMs >= 280, `the heavy script is a long task: ${m.longTasks}, ${m.longestTaskMs} ms`);
      expect(m.tbtMs >= 200, `it blocks for its time past 50 ms: ${m.tbtMs} ms`);
      expect(m.cls > 0.05, `the late banner shifts the layout: ${m.cls}`);
      expect(m.fcpMs > 0 && m.lcpMs >= m.fcpMs, `paints: first ${m.fcpMs}, largest ${m.lcpMs}`);
      expect(["good", "needs improvement", "poor"].includes(m.ratings.lcp) && m.ratings.cls, `rated: ${JSON.stringify(m.ratings)}`);
      expect(m.requests >= 1 && m.conditions.includes("1280x800"), `with its conditions: ${m.conditions}`);
    },
  },
  {
    id: "evaluate-error",
    about: "A script error comes back with its message, not just \"Uncaught\"",
    async run({ raw, call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/home.html` });
      // A popup's generated id starts with a digit, which isn't a valid selector.
      const r = await raw("evaluate", { sessionId, expression: "document.querySelector('#6ae4a8')" });
      expect(r.isError && /SyntaxError.*not a valid selector/.test(text(r)), `the error says why: ${text(r).slice(0, 160)}`);
    },
  },
  {
    id: "web-component-button",
    needs: "TYPESAFE_API_KEY",
    about: "Plain language finds a web component button whose text sits in a <slot>",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/admin.html` });
      const r = await raw("click", { sessionId, selector: "jev: the Add gift card product button" });
      expect(!r.isError, `Jev found it: ${text(r).slice(0, 200)}`);
      // Named, not guessed: unnamed buttons can still be picked by position.
      expect(text(r).includes('"Add gift card product"'), `Jev saw the button's text: ${text(r).slice(0, 200)}`);
      expect((await js(call, sessionId, "document.title")) === "clicked Add gift card product", "the right button was clicked");
    },
  },
  {
    id: "custom-dropdowns",
    about: "select_option picks from script-drawn dropdowns (long, searched, portal menus) and native selects, and reads the field back",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/apply.html` });
      const values = () => js(call, sessionId, "JSON.stringify(window.formValues)").then(JSON.parse);
      // Far down a scrolling menu drawn at the end of the body.
      const state = await call("select_option", { sessionId, selector: "#state", option: "massachusetts" });
      expect(state.chosen === "Massachusetts" && state.confirmed, `state: ${JSON.stringify(state)}`);
      const sponsor = await call("select_option", { sessionId, selector: "#sponsor", option: "No" });
      expect(sponsor.chosen === "No" && sponsor.confirmed && sponsor.shown.startsWith("No"), `sponsorship: ${JSON.stringify(sponsor)}`);
      // Only the first 50 countries are drawn until you type; the shortest match wins.
      const country = await call("select_option", { sessionId, selector: "#country", option: "United States" });
      expect(country.chosen === "United States +1" && country.confirmed, `country: ${JSON.stringify(country)}`);
      // Searched as you type, answered after a delay.
      const school = await call("select_option", { sessionId, selector: "#school", option: "Boston University" });
      expect(school.chosen === "Boston University" && school.confirmed, `school: ${JSON.stringify(school)}`);
      // A native select, by the start of its text.
      const pronouns = await call("select_option", { sessionId, selector: "#pronouns", option: "they" });
      expect(pronouns.chosen === "They/them" && pronouns.confirmed, `pronouns: ${JSON.stringify(pronouns)}`);
      const v = await values();
      expect(v.state === "Massachusetts" && v.sponsor === "No" && v.country === "United States +1" && v.school === "Boston University" && v.pronouns === "They/them",
        `the form took every choice: ${JSON.stringify(v)}`);
      // Typing is recorded, to tell whether a miss searched.
      await js(call, sessionId, "(window.__typed = [], document.addEventListener('input', (e) => window.__typed.push(e.target.id + ':' + e.target.value), true), 1)");
      const missing = await raw("select_option", { sessionId, selector: "#sponsor", option: "Maybe" });
      expect(missing.isError && text(missing).includes('"Yes", "No"'), `a missing option fails listing the offered ones: ${text(missing).slice(0, 200)}`);
      expect((await js(call, sessionId, "window.__typed.length")) === 0, "a short list shown in full isn't searched");
      const far = await raw("select_option", { sessionId, selector: "#country", option: "Atlantis" });
      expect(far.isError && text(far).includes("searched for it too"), `a long list is searched: ${text(far).slice(0, 200)}`);
      expect((await js(call, sessionId, "document.getElementById('country').value")) === "", "the search is cleared after a miss");
      // Listing chooses and types nothing.
      const before = await js(call, sessionId, "window.__typed.length");
      const listed = await call("select_option", { sessionId, selector: "#sponsor" });
      expect(listed.options?.join() === "Yes,No" && !listed.chosen, `listing returns the options: ${JSON.stringify(listed)}`);
      expect((await js(call, sessionId, "window.__typed.length")) === before && !(await js(call, sessionId, "!!document.querySelector('[role=listbox]')")),
        "listing typed nothing and closed the list");
      // An action that opens a list reports its options.
      const opened = await call("click", { sessionId, selector: "#state" });
      expect(opened.options?.[0] === "Alabama" && opened.options.length === 50, `the click reports the open list: ${JSON.stringify(opened.options?.slice(0, 3))}`);
      await call("press_key", { sessionId, key: "Escape" });
      expect((await values()).sponsor === "No", "the failed choice changed nothing");
      const notList = await raw("select_option", { sessionId, selector: "#first_name", option: "Alex" });
      expect(notList.isError && text(notList).includes("no list of options opened"), `a plain field fails: ${text(notList).slice(0, 200)}`);
      expect((await js(call, sessionId, "document.getElementById('first_name').value")) === "", "nothing was typed into the plain field");
    },
  },
  {
    id: "dropdown-widgets",
    about: "select_option on dropdowns that start late, grow in, park their input off screen, have no roles, ignore Escape, or sit in a frame",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/widgets.html` });
      const pick = async (selector, list, option) => {
        const l = await call("select_option", { sessionId, selector });
        expect(l.options?.join() === list, `${selector} lists ${list}: ${JSON.stringify(l.options)}`);
        const r = await call("select_option", { sessionId, selector, option });
        expect(r.chosen === option && r.confirmed, `${selector} picks ${option}: ${JSON.stringify(r)}`);
      };
      // Its handlers arrive a second after load: the first click does nothing.
      await pick("#late", "Free,Pro,Team", "Team");
      // Grows in and ignores clicks until it has.
      await pick("#size", "Small,Medium,Large", "Large");
      // The input is off the screen until the box around it has focus.
      await pick("#theme-search", "Light,Dark,Auto", "Dark");
      // No roles; closed it is only moved off the screen.
      const country = await call("select_option", { sessionId, selector: "#country-trigger" });
      expect(country.options?.length === 30 && country.options[0] === "United States", `the country list without roles: ${JSON.stringify(country.options?.slice(0, 3))}`);
      const c = await call("select_option", { sessionId, selector: "#country-trigger", option: "Canada" });
      expect(c.chosen === "Canada" && c.confirmed, `country picks Canada: ${JSON.stringify(c)}`);
      // No roles, and Escape doesn't close it.
      await pick("#gender", "Male,Female,Other", "Female");
      // In a frame below the fold.
      await pick("#speed", "Slower,Slow,Medium,Fast,Faster", "Fast");
      const v = JSON.parse(await js(call, sessionId, "JSON.stringify(window.formValues)"));
      expect(v.plan === "Team" && v.size === "Large" && v.theme === "Dark" && v.country === "Canada" && v.gender === "Female" && v.speed === "Fast",
        `the page took every pick: ${JSON.stringify(v)}`);
    },
  },
  {
    id: "file-upload",
    about: "upload_file attaches through a hidden input behind an Attach button, from bytes or a path, and drops onto a drop zone",
    async run({ call, raw, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/apply.html` });
      const values = () => js(call, sessionId, "JSON.stringify(window.formValues)").then(JSON.parse);
      const pdf = Buffer.from("%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n");
      const r = await call("upload_file", { sessionId, selector: "#attach", content: pdf.toString("base64"), name: "resume.pdf" });
      expect(r.how === "input" && r.into.startsWith("input#resume") && r.mimeType === "application/pdf", `it used the hidden input: ${JSON.stringify(r)}`);
      await call("wait_for", { sessionId, expression: "window.formValues.resume", timeout: 3000 });
      const got = (await values()).resume;
      expect(got.name === "resume.pdf" && got.size === pdf.length && got.type === "application/pdf" && got.head === "%PDF-", `the page read the file: ${JSON.stringify(got)}`);
      // A file the page's accept list doesn't take still goes in, with a warning.
      const png = await raw("upload_file", { sessionId, selector: "#resume", content: "iVBORw0KGgo=", name: "photo.png" });
      expect(text(png).includes("only accepts"), `it warns about accept: ${text(png).slice(0, 300)}`);
      // From a path on the server's machine.
      const file = path.join(mkdtempSync(path.join(tmpdir(), "bs-upload-")), "cv.txt");
      writeFileSync(file, "Plain text CV");
      const fromPath = await call("upload_file", { sessionId, selector: "#resume", path: file });
      await call("wait_for", { sessionId, expression: "window.formValues.resume.name === 'cv.txt'", timeout: 3000 });
      expect(fromPath.pageShowsName && (await values()).resume.size === 13, `a path works: ${JSON.stringify(fromPath)}`);
      // No file input behind it: dropped.
      const cover = await call("upload_file", { sessionId, selector: "#cover-drop", content: Buffer.from("Dear team").toString("base64"), name: "cover.txt" });
      await call("wait_for", { sessionId, expression: "window.formValues.cover", timeout: 3000 });
      expect(cover.how === "drop" && (await values()).cover.text === "Dear team", `the drop zone took it: ${JSON.stringify(cover)}`);
      const none = await raw("upload_file", { sessionId, selector: "#resume", content: "AAAA" });
      expect(none.isError && text(none).includes("needs a file `name`"), "content without a name fails");
    },
  },
  {
    id: "cross-site-frame",
    about: "An application form embedded from another site: snapshot, type, click, select_option, upload_file, press_key, wait_for and evaluate reach into the frame",
    async run({ call, raw, session }) {
      if (!LAN_SITE) throw new Skipped("no non-loopback address to serve the frame from");
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/embed.html` });
      // The form from another origin, and beside it a tracking pixel from the
      // same site, which holds nothing to act on.
      await call("evaluate", {
        sessionId,
        expression: `(() => {
          document.getElementById("embed").src = ${JSON.stringify(`${LAN_SITE}/apply.html`)};
          const pixel = Object.assign(document.createElement("iframe"), { src: ${JSON.stringify(`${LAN_SITE}/home.html`)} });
          pixel.style.cssText = "width:1px;height:1px;border:0;padding:0;position:absolute;left:0;top:0";
          document.body.append(pixel);
          return true;
        })()`,
      });
      await call("wait_for", { sessionId, expression: "Object.keys(window.embedValues).length > 0", timeout: 5000 });
      await call("wait_for", { sessionId, ms: 500 });
      const values = () => js(call, sessionId, "JSON.stringify(window.embedValues)").then(JSON.parse);
      const snap = text(await raw("take_snapshot", { sessionId }));
      expect(snap.includes("Software Engineer application"), `the snapshot shows the frame's form: ${snap.slice(0, 400)}`);
      await call("type", { sessionId, selector: "#first_name", text: "Test" });
      // A script-drawn dropdown far down the frame, and a native select.
      const state = await call("select_option", { sessionId, selector: "#state", option: "Massachusetts" });
      expect(state.chosen === "Massachusetts" && state.confirmed, `state: ${JSON.stringify(state)}`);
      const pronouns = await call("select_option", { sessionId, selector: "#pronouns", option: "She/her" });
      expect(pronouns.chosen === "She/her", `pronouns: ${JSON.stringify(pronouns)}`);
      // A click opens a list there, and the result lists its options.
      const opened = await call("click", { sessionId, selector: "#sponsor" });
      expect((opened.options || []).includes("No"), `the click reports the list: ${JSON.stringify(opened)}`);
      await call("press_key", { sessionId, key: "Escape" });
      const pdf = Buffer.from("%PDF-1.4\n%%EOF\n").toString("base64");
      const cv = await call("upload_file", { sessionId, selector: "#attach", content: pdf, name: "resume.pdf" });
      expect(cv.how === "input" && cv.into.startsWith("input#resume"), `upload: ${JSON.stringify(cv)}`);
      await call("wait_for", { sessionId, expression: "window.embedValues.resume && window.embedValues.first_name", timeout: 3000 });
      const v = await values();
      expect(v.first_name === "Test", `typed into the frame: ${JSON.stringify(v)}`);
      expect(v.state === "Massachusetts" && v.pronouns === "She/her", `picked in the frame: ${JSON.stringify(v)}`);
      expect(v.resume?.name === "resume.pdf" && v.resume?.head === "%PDF-", `attached in the frame: ${JSON.stringify(v.resume)}`);
      // Focus inside the frame is the field there, not the <iframe>.
      await call("click", { sessionId, selector: "#first_name" });
      const tab = await call("press_key", { sessionId, key: "Tab" });
      expect(!tab.focused.startsWith("iframe") && tab.focused.includes("input"), `press_key sees the frame's focus: ${tab.focused}`);
      // Text only the frame shows, and a script run in the frame.
      await call("wait_for", { sessionId, text: "Software Engineer application", timeout: 3000 });
      const host = new URL(LAN_SITE).host;
      const read = await call("evaluate", { sessionId, frame: host, expression: "document.querySelector('#first_name').value" });
      expect(read.result === "Test", `evaluate in the frame: ${JSON.stringify(read)}`);
      const none = await raw("evaluate", { sessionId, frame: "no-such-host", expression: "1" });
      expect(none.isError && text(none).includes(host), `a missing frame lists the frames: ${text(none).slice(0, 200)}`);
      // Plain language reaches the frame's fields too.
      if (process.env.TYPESAFE_API_KEY) {
        const sponsor = await call("select_option", { sessionId, selector: "Will you now or in the future require sponsorship?", option: "No" });
        expect(sponsor.chosen === "No", `plain language: ${JSON.stringify(sponsor)}`);
        await call("wait_for", { sessionId, expression: "window.embedValues.sponsor === 'No'", timeout: 3000 });
      }
    },
  },
  {
    id: "late-file-name",
    about: "upload_file sees a file name the page shows only once its upload finishes, in capitals",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/questions.html` });
      const r = await call("upload_file", { sessionId, selector: "#cv-drop", content: Buffer.from("%PDF-1.4\n").toString("base64"), name: "My CV.pdf" });
      expect(r.into.startsWith("input#cv-input") && r.pageShowsName, `it saw the name: ${JSON.stringify(r)}`);
    },
  },
  {
    id: "question-labels",
    needs: "TYPESAFE_API_KEY",
    about: "Plain language finds fields by the question beside them when the page links no label to them",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/questions.html` });
      const values = () => js(call, sessionId, "JSON.stringify(window.formValues)").then(JSON.parse);
      const school = await call("select_option", { sessionId, selector: "Which school did you last attend?", option: "Boston University" });
      const source = await call("select_option", { sessionId, selector: "How did you hear about this job?", option: "LinkedIn" });
      // Two Yes/No dropdowns only their questions tell apart.
      await call("select_option", { sessionId, selector: "Are you willing to relocate?", option: "Yes" });
      await call("select_option", { sessionId, selector: "Will you need a visa to work here?", option: "No" });
      const yn = await values();
      expect(yn["move-select"] === "Yes" && yn["visa-select"] === "No", `yes/no: ${JSON.stringify(yn)}`);
      const place = await call("select_option", { sessionId, selector: "Where are you located?", option: "Boston, Massachusetts" });
      const cv = await call("upload_file", { sessionId, selector: "Resume", content: Buffer.from("%PDF-1.4\n").toString("base64"), name: "cv.pdf" });
      // A one pixel input whose question reaches it only as its label's description.
      const letter = await call("upload_file", { sessionId, selector: "Cover letter", content: Buffer.from("Dear team").toString("base64"), name: "letter.txt" });
      expect(letter.into.startsWith("input#letter-input"), `cover letter: ${JSON.stringify(letter)}`);
      await call("wait_for", { sessionId, expression: "window.formValues.cv", timeout: 3000 });
      const v = await values();
      expect(v["school-select"] === "Boston University", `school: ${JSON.stringify(school)}`);
      expect(v["source-select"] === "LinkedIn", `source: ${JSON.stringify(source)}`);
      expect(v.location === "Boston, Massachusetts, United States", `location: ${JSON.stringify(place)}`);
      expect(v.cv === "cv.pdf" && cv.into.startsWith("input#cv-input"), `resume: ${JSON.stringify(cv)}`);
    },
  },
  {
    id: "drop-zones",
    needs: "TYPESAFE_API_KEY",
    about: "Plain language finds a drop zone with a file and a folder input, and one with no input at all",
    async run({ call, session }) {
      const sessionId = await session();
      await call("navigate", { sessionId, url: `${SITE}/questions.html` });
      const values = () => js(call, sessionId, "JSON.stringify(window.formValues)").then(JSON.parse);
      const b64 = Buffer.from("sample").toString("base64");
      const pf = await call("upload_file", { sessionId, selector: "Drop files here for the portfolio", content: b64, name: "work.pdf" });
      expect(pf.into.startsWith("input#pf-files") && (await values()).portfolio === "work.pdf" && !(await values()).folder, `portfolio: ${JSON.stringify(pf)}`);
      const sample = await call("upload_file", { sessionId, selector: "the writing sample drop zone", content: b64, name: "essay.txt" });
      expect(sample.how === "drop" && (await values()).sample === "essay.txt", `writing sample: ${JSON.stringify(sample)}`);
    },
  },
  {
    id: "takeover-wait",
    needs: "TAKEOVER_BASE_URL",
    about: "await_human_takeover returns at its timeout, not finished and not expired",
    async run({ call, session }) {
      const sessionId = await session();
      const { token } = await call("request_human_takeover", { sessionId, ttlMinutes: 1 });
      const t0 = Date.now();
      const r = await call("await_human_takeover", { token, timeout: 1500 });
      expect(Date.now() - t0 < 5000, "it returns at its timeout");
      expect(r.completed === false && r.expired === false, `not finished, not expired: ${JSON.stringify(r)}`);
    },
  },
];

// ---- runner ----------------------------------------------------------------
const arg = (k, d) => {
  const i = process.argv.indexOf(k);
  return i > 0 ? process.argv[i + 1] : d;
};
const only = arg("--only")?.split(",");
const { call, raw, close } = await connect();
let failed = 0;
for (const s of SCENARIOS.filter((s) => !only || only.includes(s.id))) {
  const opened = [];
  const session = async (args = {}) => {
    const { sessionId } = await call("open_browser_session", args);
    opened.push(sessionId);
    return sessionId;
  };
  const t0 = Date.now();
  let mark = "✓";
  let note = s.about;
  try {
    if (s.needs && !process.env[s.needs]) throw new Skipped(`needs ${s.needs}`);
    // A hung call (a page dialog nobody answers) fails the scenario, not the run.
    let timer;
    await Promise.race([
      s.run({ call, raw, session, opened }),
      new Promise((_, reject) => (timer = setTimeout(() => reject(new Failed("hung for 60s")), 60_000))),
    ]).finally(() => clearTimeout(timer));
  } catch (e) {
    if (e instanceof Skipped) [mark, note] = ["-", `skipped: ${e.message}`];
    else {
      [mark, note] = ["✗", e.message.slice(0, 300)];
      failed++;
    }
  } finally {
    for (const id of opened) {
      await Promise.race([
        call("close_browser_session", { sessionId: id }).catch(() => {}),
        new Promise((r) => setTimeout(r, 5_000)),
      ]);
    }
  }
  console.log(`${mark} ${s.id.padEnd(22)} ${((Date.now() - t0) / 1000).toFixed(1)}s  ${note}`);
}
close();
server.close();
chrome?.kill();
process.exit(failed ? 1 : 0);
