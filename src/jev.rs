//! `jev:` selectors — let the Jev decision model pick the element.
//!
//! Jev (TypeSafe AI's "System One" decision model) can't plan or write text,
//! but it is very good and very cheap at picking one option out of a bounded
//! set: roughly $0.042 per million input tokens and 150–500ms per call, where
//! a driving LLM would read a whole snapshot and reason just to name a CSS
//! selector. So the `jev:` selector prefix replaces exactly one decision —
//! WHICH element — and leaves planning, text and verification to the calling
//! agent:
//!
//! ```text
//! click({ sessionId, selector: "jev: the login button" })
//!   1. extract the page's interactive elements from the DOM, each stamped
//!      with a data-jev-id attribute (in-viewport first, cap 240 — below
//!      Jev's 255-option Choice limit)
//!   2. one Choice call over those ids, goal in the state string
//!   3. act on [data-jev-id="<stamp>-e17"] through the same trusted-CDP path
//! ```
//!
//! The DOM stamp carries a random per-extraction token: a concurrent
//! extraction on the same tab (another `act`, another MCP process) re-stamps
//! the page, and a stale pick then matches nothing and fails closed instead
//! of landing on whichever element became "e17" in the newer menu.
//!
//! Everything fails closed: no key configured, empty page, low confidence,
//! malformed answer — the tool errors and the caller falls back to a CSS
//! selector. A Jev answer never turns into coordinates or JS on its own; it
//! only selects among ids the extraction just offered.
//!
//! Stealth note: extraction runs in an isolated world, but the attribute
//! stamps are DOM mutations a page script could observe through a
//! MutationObserver — the same trade-off every Jev browser integration makes.
use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::Page;
use once_cell::sync::Lazy;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

use crate::server::iso_eval_function;

/// Jev's limit on the options of one Choice question.
const CHOICE_LIMIT: usize = 255;

/// How many interactive elements one extraction collects. A page with more
/// than fit one Choice is narrowed in even groups first (see [`narrow`]);
/// past this, the rest are left out and Jev is told how many.
const MAX_ELEMENTS: usize = 5000;

/// Group questions per request in [`narrow`]: each holds up to 254 element
/// descriptions, and four keep a request well inside Jev's 64k-token
/// context. Requests beyond the first go out in parallel.
const GROUPS_PER_REQUEST: usize = 4;

/// Collect interactive elements from the DOM — not the accessibility tree,
/// which under-reports inputs (DuckDuckGo's search box disappears from it,
/// for one; both jev-browser and jev-ultrafast hit this and switched to the
/// DOM). Clears stale `data-jev-id` stamps from earlier calls so a
/// `[data-jev-id="eN"]` selector can only ever match an element Jev was just
/// offered, then re-stamps in offer order: in-viewport elements first so the
/// cap can't crowd them out on long pages. [`MAX_ELEMENTS`] is injected into
/// the template below so the Rust and JS caps can't drift apart; the stamp
/// token is injected per call (see [`extract_fn`]).
const EXTRACT_FN_TEMPLATE: &str = r#"function() {
  var MAX = __MAX_ELEMENTS__;
  var STAMP = '__STAMP__';
  var SKIP_TYPED = __SKIP_TYPED__;
  var SEL = 'a[href], button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="option"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [role="slider"], [onclick], [oncontextmenu], [ondblclick], [draggable="true"], [tabindex]:not([tabindex="-1"])';
  var clamp = function(s, n) {
    s = String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
    var a = Array.from(s);
    return a.length > n ? a.slice(0, Math.max(0, n - 1)).join('') + '...' : s;
  };
  var describe = function(el) {
    var tag = el.tagName.toLowerCase();
    var p = [tag];
    if (el.id) p.push('#' + clamp(el.id, 40));
    var role = el.getAttribute('role');
    if (role) p.push('role=' + clamp(role, 24));
    if (el.isContentEditable) p.push('[contenteditable]');
    if (el.getAttribute('draggable') === 'true') p.push('[draggable]');
    if (el.hasAttribute('oncontextmenu')) p.push('[has context menu]');
    if (tag === 'input') {
      var t = el.getAttribute('type');
      if (t) p.push('type=' + clamp(t, 16));
    }
    // A checkbox or radio's `value` is form data ("on" by default), not a
    // label; unlabeled ones often sit next to bare text instead
    // (`<input type=checkbox> checkbox 1`).
    var toggle = tag === 'input' && (el.type === 'checkbox' || el.type === 'radio');
    var adjacent = function() {
      var n = el.nextSibling;
      while (n && n.nodeType === 3 && !n.textContent.trim()) n = n.nextSibling;
      return n && n.nodeType === 3 ? n.textContent : '';
    };
    // Fields a person types into. Their contents are user data (a typed card
    // number, an autofilled email), so they never become the label sent to
    // Jev; only whether they hold anything does. Button-type inputs keep
    // their value: there it is the button's text.
    var typed = tag === 'textarea' || el.isContentEditable || (tag === 'input' &&
      ['button', 'submit', 'reset', 'image', 'file', 'checkbox', 'radio', 'hidden',
        'range', 'color'].indexOf(el.type) < 0);
    if (el.type === 'password') {
      p.push('(value hidden)');
    } else if (typed) {
      var name = el.getAttribute('aria-label') || el.getAttribute('placeholder') ||
        el.getAttribute('title') ||
        (el.labels && el.labels[0] ? el.labels[0].innerText : '');
      if (name) p.push('"' + clamp(name, 80) + '"');
    } else {
      var label = el.getAttribute('aria-label') || el.getAttribute('placeholder') ||
        el.getAttribute('title') ||
        (el.labels && el.labels[0] ? el.labels[0].innerText : '') ||
        el.innerText || (toggle ? adjacent() : el.value || el.getAttribute('value')) || '';
      if (label) p.push('"' + clamp(label, 80) + '"');
    }
    if (typed || el.type === 'password') {
      var content = el.isContentEditable ? el.innerText : el.value;
      p.push(String(content || '').trim() ? '(filled)' : '(empty)');
    }
    // Current state, so a goal like "check the box" can read as met and a
    // toggle isn't clicked again (and undone) on the next step.
    if (toggle) p.push(el.checked ? '(checked)' : '(unchecked)');
    var aria = { 'aria-checked': 'checked', 'aria-pressed': 'pressed',
      'aria-expanded': 'expanded', 'aria-selected': 'selected' };
    for (var attr in aria) {
      var v = el.getAttribute(attr);
      if (v === 'true') p.push('(' + aria[attr] + ')');
      else if (v === 'false') p.push('(not ' + aria[attr] + ')');
    }
    if (tag === 'select' && el.selectedOptions && el.selectedOptions[0]) {
      p.push('selected="' + clamp(el.selectedOptions[0].label, 40) + '"');
    }
    if (tag === 'a' && el.getAttribute('href')) p.push('-> ' + clamp(el.getAttribute('href'), 80));
    return p.join(' ');
  };
  // Every document the page shows: the top one, open shadow roots and
  // same-origin iframes, each with its offset from the top viewport.
  var roots = [];
  var walk = function(root, ox, oy, depth) {
    if (depth > 4) return;
    roots.push({ root: root, ox: ox, oy: oy });
    var all = root.querySelectorAll('*');
    for (var a = 0; a < all.length && a < 20000; a++) {
      var n = all[a];
      if (n.shadowRoot) walk(n.shadowRoot, ox, oy, depth + 1);
      if (n.tagName === 'IFRAME' || n.tagName === 'FRAME') {
        var doc = null;
        try { doc = n.contentDocument; } catch (e) {}
        if (doc && doc.documentElement) {
          var fr = n.getBoundingClientRect();
          walk(doc, ox + fr.left + n.clientLeft, oy + fr.top + n.clientTop, depth + 1);
        }
      }
    }
  };
  walk(document, 0, 0, 0);
  var styleOf = function(el) { return (el.ownerDocument.defaultView || window).getComputedStyle(el); };
  var DIALOG = 'dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]';
  var vw = document.documentElement.clientWidth || 0;
  var vh = document.documentElement.clientHeight || 0;
  var inDialog = [], inView = [], offView = [], seen = new Set();
  var consider = function(el, ox, oy) {
    if (seen.has(el) || el.disabled) return;
    seen.add(el);
    // A field this act run already filled stays out of the offer while it
    // holds text, so the loop moves on instead of re-picking it.
    if (SKIP_TYPED && el.hasAttribute('data-bs-typed') && String(el.value || el.innerText || '').trim()) return;
    if (!el.getClientRects().length) return;
    var st = styleOf(el);
    if (st.visibility === 'hidden' || st.display === 'none') return;
    var r = el.getBoundingClientRect();
    var visible = r.bottom + oy > -20 && r.top + oy < vh + 20 && r.right + ox > -20 && r.left + ox < vw + 20;
    if (visible && el.closest(DIALOG)) inDialog.push(el);
    else (visible ? inView : offView).push(el);
  };
  // Elements only scripts make clickable (sortable table headers, card
  // tiles): pointer cursor, no interactive ancestor or descendant, and not
  // just inheriting the cursor from a parent.
  var POINTER = 'th, td, li, span, div, img, label, svg, i, p';
  for (var ri = 0; ri < roots.length; ri++) {
    var R = roots[ri];
    var stale = R.root.querySelectorAll('[data-jev-id]');
    for (var k = 0; k < stale.length; k++) stale[k].removeAttribute('data-jev-id');
    var nodes = R.root.querySelectorAll(SEL);
    for (var i = 0; i < nodes.length; i++) consider(nodes[i], R.ox, R.oy);
    // Table headers (often sortable) and sizeable images (hover targets).
    var extra = R.root.querySelectorAll('thead th, img');
    for (var x = 0; x < extra.length && x < 3000; x++) {
      var xe = extra[x];
      if (xe.tagName === 'IMG') {
        if (xe.closest('a, button')) continue;
        var xr = xe.getBoundingClientRect();
        if (xr.width < 24 || xr.height < 24) continue;
      }
      consider(xe, R.ox, R.oy);
    }
    var maybe = R.root.querySelectorAll(POINTER);
    for (var m = 0; m < maybe.length && m < 3000; m++) {
      var c = maybe[m];
      if (c.closest(SEL) || c.querySelector(SEL)) continue;
      if (styleOf(c).cursor !== 'pointer') continue;
      if (c.parentElement && styleOf(c.parentElement).cursor === 'pointer') continue;
      consider(c, R.ox, R.oy);
    }
  }
  var chosen = inDialog.concat(inView, offView).slice(0, MAX);
  var elements = [];
  for (var j = 0; j < chosen.length; j++) {
    var id = 'e' + (j + 1);
    chosen[j].setAttribute('data-jev-id', STAMP + '-' + id);
    var desc = describe(chosen[j]);
    if (chosen[j].closest(DIALOG)) desc += ' (in dialog)';
    var rec = { id: id, desc: desc };
    if (chosen[j].tagName === 'SELECT' && chosen[j].options) {
      var opts = [];
      for (var q = 0; q < chosen[j].options.length && q < 25; q++) {
        opts.push({ i: q, label: clamp(chosen[j].options[q].label, 60) });
      }
      rec.options = opts;
    }
    elements.push(rec);
  }
  var textOf = function(root) {
    var t = String(root.innerText || '').replace(/\s+/g, ' ').trim();
    return t.length > 1500 ? t.slice(0, 1499) + '...' : t;
  };
  var modal = document.querySelector(DIALOG);
  var text = '';
  if (modal && modal.getClientRects().length) text = textOf(modal);
  else if (document.body) text = textOf(document.body);
  return {
    url: String(location.href),
    title: clamp(document.title, 120),
    text: text,
    elements: elements,
    omitted: Math.max(0, inDialog.length + inView.length + offView.length - chosen.length)
  };
}"#;

/// The extraction script, with [`MAX_ELEMENTS`] baked in.
static EXTRACT_FN: Lazy<String> =
    Lazy::new(|| EXTRACT_FN_TEMPLATE.replace("__MAX_ELEMENTS__", &MAX_ELEMENTS.to_string()));

/// The extraction script for one call, stamping with `stamp` (hex only, so
/// it is safe inside the JS string literal).
fn extract_fn(stamp: &str, skip_typed: bool) -> String {
    EXTRACT_FN
        .replace("__STAMP__", stamp)
        .replace("__SKIP_TYPED__", if skip_typed { "true" } else { "false" })
}

/// A fresh random stamp token for one extraction.
fn new_stamp() -> Result<String> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// JS that finds the first element matching a CSS selector in the page or,
/// failing that, in any open shadow root or same-origin iframe (where `act`
/// stamps elements too), with the offset of its document from the top
/// viewport. Defines `__bsDeep(sel)` → `{ el, ox, oy }` or `null`; prepend
/// it to a function body.
pub const DEEP_FIND_JS: &str = r#"var __bsDeep = function(sel) {
  var hit = null;
  var walk = function(root, ox, oy, depth) {
    if (hit || depth > 4) return;
    var el = root.querySelector(sel);
    if (el) { hit = { el: el, ox: ox, oy: oy }; return; }
    var all = root.querySelectorAll('*');
    for (var a = 0; a < all.length && a < 20000 && !hit; a++) {
      var n = all[a];
      if (n.shadowRoot) walk(n.shadowRoot, ox, oy, depth + 1);
      if (!hit && (n.tagName === 'IFRAME' || n.tagName === 'FRAME')) {
        var doc = null;
        try { doc = n.contentDocument; } catch (e) {}
        if (doc && doc.documentElement) {
          var fr = n.getBoundingClientRect();
          walk(doc, ox + fr.left + n.clientLeft, oy + fr.top + n.clientTop, depth + 1);
        }
      }
    }
  };
  walk(document, 0, 0, 0);
  return hit;
};"#;

/// A cheap fingerprint of what an action could visibly change: location,
/// title, visible text, the interactive-element count, open dialogs,
/// expanded/checked/selected states and form values. `act` compares it
/// before and after each action, so a modal opening, a checkbox ticking or
/// a single-page-app re-render counts as an effect even when the URL and
/// title stay put. Hashed in the page: form values never leave it.
pub const FINGERPRINT_FN: &str = r#"function() {
  var h = 2166136261;
  var mix = function(s) {
    s = String(s == null ? '' : s);
    for (var i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
    h ^= 31; h = Math.imul(h, 16777619);
  };
  mix(location.href);
  mix(document.title);
  mix(document.body ? document.body.innerText : '');
  mix(document.querySelectorAll('dialog[open], [role="dialog"], [aria-modal="true"]').length);
  var els = document.querySelectorAll('a[href], button, input, textarea, select, summary, details, [contenteditable="true"], [role], [aria-expanded], [aria-checked], [aria-selected], [aria-pressed]');
  mix(els.length);
  for (var k = 0; k < els.length; k++) {
    var el = els[k];
    mix((el.getAttribute('aria-expanded') || '') + (el.getAttribute('aria-checked') || '') +
      (el.getAttribute('aria-selected') || '') + (el.getAttribute('aria-pressed') || '') +
      (el.checked ? 'c' : '') + (el.open ? 'o' : '') +
      (el.value !== undefined && el.type !== 'password' ? el.value : ''));
  }
  return (h >>> 0).toString(16);
}"#;

/// One resolved `jev:` goal: everything the caller needs to act and to tell
/// the agent what was picked.
pub struct Resolution {
    /// The chosen element's offered id, e.g. "e17".
    pub element_id: String,
    /// CSS selector targeting it, e.g. `[data-jev-id="1a2b3c4d-e17"]`.
    pub css: String,
    /// The chosen element's compact description, for the confirmation text.
    pub description: String,
    /// Jev's calibrated confidence in the choice.
    pub confidence: f64,
    /// How many elements were offered.
    pub offered: usize,
}

struct Config {
    api_key: String,
    base_url: String,
    model: String,
    timeout: Duration,
    min_confidence: f64,
    fallback_min_confidence: f64,
}

/// Read config lazily, per call: `jev:` is an optional feature, so a missing
/// key must degrade to a clear tool error at call time, not a startup failure
/// of the whole server.
fn config() -> Result<Config> {
    let api_key = std::env::var("TYPESAFE_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        bail!(
            "`jev:` selectors need a TypeSafe API key. Set TYPESAFE_API_KEY on the server \
             (keys: console.typesafe.ai) or pass a plain CSS selector"
        );
    }
    Ok(Config {
        api_key,
        // Point at a Jev-compatible carrier (Vercel AI Gateway, OpenRouter,
        // Cloudflare Workers AI) by changing only this.
        base_url: std::env::var("JEV_BASE_URL")
            .unwrap_or_else(|_| "https://api.typesafe.ai".to_string()),
        // Pinned, not `jev-latest`: answers can shift between Jev releases,
        // so upgrades should be deliberate.
        model: std::env::var("JEV_MODEL").unwrap_or_else(|_| "jev-1.14.0".to_string()),
        timeout: Duration::from_millis(env_f64("JEV_TIMEOUT_MS", 10_000.0).max(1.0) as u64),
        min_confidence: env_f64("JEV_MIN_CONFIDENCE", 0.5).clamp(0.0, 1.0),
        // Stricter by default: when the caller passed CSS that matched nothing,
        // a Jev guess is a rescue, not the asked-for behavior — it should only
        // fire when Jev is clearly right. An explicit `jev:` goal keeps the
        // lower JEV_MIN_CONFIDENCE floor, since there the caller asked for a
        // best guess. (Jev happily picks a confident-but-wrong in-set option
        // for an impossible goal — measured at 0.81 on a nonsense target.)
        fallback_min_confidence: env_f64("JEV_FALLBACK_MIN_CONFIDENCE", 0.85).clamp(0.0, 1.0),
    })
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Whether Jev can be used at all (a TypeSafe API key is set). Gates the
/// automatic fallback that retries selectors which match nothing as CSS;
/// without a key that fallback is skipped and the original CSS error is
/// returned unchanged.
pub fn available() -> bool {
    !std::env::var("TYPESAFE_API_KEY")
        .unwrap_or_default()
        .is_empty()
}

/// One option of an extracted `<select>`, identified by live DOM index —
/// labels clamp and can collide, indexes cannot (and long labels could
/// otherwise never be matched after clamping).
#[derive(Clone)]
pub struct SelectOption {
    pub i: u64,
    pub label: String,
}

/// One extracted interactive element.
#[derive(Clone)]
pub struct Element {
    /// Its offered id, e.g. "e17" (the DOM stamp prefixes the extraction's
    /// token; see [`PageInfo::css`]).
    pub id: String,
    /// Compact description, as offered to Jev.
    pub desc: String,
    /// For `<select>` elements: the options (labels capped), in order.
    pub options: Vec<SelectOption>,
}

/// A page extraction: what a Jev decision was offered.
pub struct PageInfo {
    pub url: String,
    pub title: String,
    /// Visible page text, modal-first: the innermost open dialog when one
    /// is showing, else the body. Gives goal-met / login / error judgments
    /// page content to read instead of judging on URL and title alone.
    pub text: String,
    /// Elements in offer order.
    pub elements: Vec<Element>,
    /// Interactive elements beyond [`MAX_ELEMENTS`], not offered.
    pub omitted: usize,
    /// This extraction's stamp token.
    pub stamp: String,
}

impl PageInfo {
    /// The same extraction with only the elements at `keep` (in order).
    fn subset(&self, keep: &[usize]) -> PageInfo {
        PageInfo {
            url: self.url.clone(),
            title: self.title.clone(),
            text: self.text.clone(),
            elements: keep.iter().map(|&i| self.elements[i].clone()).collect(),
            omitted: self.omitted,
            stamp: self.stamp.clone(),
        }
    }

    /// CSS selector for an element this extraction offered. Matches nothing
    /// once a newer extraction has re-stamped the page.
    pub fn css(&self, id: &str) -> String {
        format!("[data-jev-id=\"{}-{id}\"]", self.stamp)
    }
}

/// The `state` string sent to Jev: the goal, the page's identity, and the
/// numbered element menu. Kept lean — Jev's accuracy drops when irrelevant
/// text rides along — so no snapshot dump, just the interactive elements.
fn build_state(goal: &str, page: &PageInfo) -> String {
    let mut lines = vec![format!("The agent wants to act on: \"{goal}\".")];
    if page.title.is_empty() {
        lines.push(format!("Page: {}", page.url));
    } else {
        lines.push(format!("Page: {} — {}", page.url, page.title));
    }
    if !page.text.is_empty() {
        lines.push(format!("Visible text: {}", page.text));
    }
    if !page.elements.is_empty() {
        lines.push("Interactive elements:".to_string());
    }
    lines.extend(
        page.elements
            .iter()
            .map(|e| format!("{}: {}", e.id, e.desc)),
    );
    if page.omitted > 0 {
        lines.push(format!(
            "({} further elements not listed; scroll if the target may be among them)",
            page.omitted
        ));
    }
    lines.join("\n")
}

/// A parsed — and validated — Choice answer.
pub struct ChoiceAnswer {
    pub chosen: String,
    /// Jev's calibrated confidence in the choice.
    pub confidence: f64,
    /// The remaining offered options ranked by probability (descending),
    /// when the response carried a distribution. `act` uses this to switch
    /// to the next-best element after a no-effect action.
    pub ranked: Vec<(String, f64)>,
}

/// Pull a Choice answer for `question` out of a `/v1/systemone` response and
/// validate it against the options actually offered. An answer naming anything
/// else — or a response without one — is an error, never a guess. When a
/// probability distribution is present it must name only offered options,
/// hold finite non-negative values, sum to ~1, and crown the chosen option
/// (jev-ultrafast's rules: a distribution that disagrees with its own choice
/// is a malformed answer). A missing distribution skips those checks and
/// only the chosen option is validated.
fn parse_choice(body: &Value, question: &str, valid: &[&str]) -> Result<ChoiceAnswer> {
    let answer = body
        .pointer(&format!("/answers/{question}"))
        .ok_or_else(|| anyhow!("Jev response has no answers.{question}"))?;
    let chosen = answer
        .get("choice")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Jev answer for {question} has no choice"))?;
    if !valid.contains(&chosen) {
        bail!("Jev chose `{chosen}`, which is not one of the offered elements");
    }
    let mut ranked: Vec<(String, f64)> = Vec::new();
    if let Some(probs) = answer.get("probabilities").and_then(|p| p.as_object()) {
        if !probs.is_empty() {
            let mut sum = 0.0f64;
            let mut top: Option<(String, f64)> = None;
            for (k, v) in probs {
                let p = v.as_f64().ok_or_else(|| {
                    anyhow!(
                        "Jev probabilities for {question} include a non-numeric value for `{k}`"
                    )
                })?;
                if !p.is_finite() || p < 0.0 {
                    bail!("Jev probabilities for {question} include an invalid value for `{k}`");
                }
                if !valid.contains(&k.as_str()) {
                    bail!("Jev probabilities for {question} name `{k}`, which was not offered");
                }
                sum += p;
                if top.as_ref().is_none_or(|(_, tp)| p > *tp) {
                    top = Some((k.clone(), p));
                }
            }
            if (sum - 1.0).abs() > 0.02 {
                bail!("Jev probabilities for {question} sum to {sum:.3}, not ~1");
            }
            // A tie (within rounding) with the chosen option is agreement:
            // `top` keeps the first of equal values, which need not be the
            // one Jev named.
            if let Some((best, best_p)) = &top {
                let chosen_p = probs.get(chosen).and_then(|v| v.as_f64()).unwrap_or(0.0);
                if best.as_str() != chosen && best_p - chosen_p > 0.01 {
                    bail!("Jev chose `{chosen}` but its own probabilities favor `{best}`");
                }
            }
            ranked = probs
                .iter()
                .filter(|(k, _)| k.as_str() != chosen)
                .filter_map(|(k, v)| v.as_f64().map(|p| (k.clone(), p)))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    // Choice answers carry confidence; fall back to the chosen option's
    // probability if a carrier omits it, else fail the confidence gate.
    let confidence = answer
        .get("confidence")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            answer
                .get("probabilities")
                .and_then(|p| p.get(chosen))
                .and_then(|v| v.as_f64())
        })
        .unwrap_or(0.0);
    Ok(ChoiceAnswer {
        chosen: chosen.to_string(),
        confidence,
        ranked,
    })
}

/// Pull a Noul (yes-probability) answer for `question` from a response.
fn parse_noul(body: &Value, question: &str) -> Result<f64> {
    body.pointer(&format!("/answers/{question}/noul"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("Jev response has no answers.{question}.noul"))
}

/// The shared HTTP client. TLS is rustls (the only TLS feature enabled), the
/// same client basis chrome.rs relies on for its https:// DevTools probe.
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("reqwest client builds")
});

/// Resolve a `jev:` goal to one element on the page. The caller explicitly
/// asked for a Jev decision, so the standard `JEV_MIN_CONFIDENCE` floor
/// (best-guess semantics) applies.
pub async fn resolve(page: &Page, goal: &str) -> Result<Resolution> {
    let floor = config()?.min_confidence;
    resolve_with_floor(page, goal, floor).await
}

/// Resolve a goal on the automatic fallback path (a selector that matched
/// nothing as CSS). Applies the stricter `JEV_FALLBACK_MIN_CONFIDENCE` floor:
/// the caller asked for CSS, so a Jev guess should only act when it is
/// clearly right.
pub async fn resolve_fallback(page: &Page, goal: &str) -> Result<Resolution> {
    let floor = config()?.fallback_min_confidence;
    resolve_with_floor(page, goal, floor).await
}

/// Run the DOM extraction with a few retries. Right after an executed
/// action the page may be mid-navigation — the previous document tearing
/// down, or the next one not yet interactive — and the injected call can
/// throw or return a half-built DOM for a moment. A short pause-and-retry
/// rides those races out; only a page that stays broken errors. An empty
/// (but parsed) result is returned as-is: "nothing interactive" is a real
/// answer the callers handle themselves.
async fn extract_with_retry(page: &Page, skip_typed: bool) -> Result<PageInfo> {
    const ATTEMPTS: usize = 3;
    const PAUSE_MS: u64 = 400;
    let mut last: Option<anyhow::Error> = None;
    let stamp = new_stamp()?;
    let script = extract_fn(&stamp, skip_typed);
    for attempt in 1..=ATTEMPTS {
        match iso_eval_function(page, &script).await {
            Ok(raw) => match parse_extraction(&raw, &stamp) {
                Ok(info) => return Ok(info),
                Err(e) => last = Some(e),
            },
            Err(e) => last = Some(anyhow!("{e:#}")),
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(PAUSE_MS)).await;
        }
    }
    Err(last.expect("at least one attempt ran")).context("extracting interactive elements")
}

/// Split `n` elements into the fewest groups of at most `cap`, all as
/// equal as possible (1,800 at 254 → 8 groups of 225), so every group
/// question uses its options alike.
fn even_groups(n: usize, cap: usize) -> Vec<std::ops::Range<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let k = n.div_ceil(cap);
    let (base, extra) = (n / k, n % k);
    let mut start = 0;
    (0..k)
        .map(|g| {
            let len = base + usize::from(g < extra);
            let r = start..start + len;
            start += len;
            r
        })
        .collect()
}

/// Narrow a page with more elements than fit one Choice (next to `reserved`
/// other options) to the few worth the final pick. The elements are split
/// into even groups of up to 254, and each group is asked, in one fan-out
/// request per [`GROUPS_PER_REQUEST`] groups, for its best match or "none
/// of these"; each group's top two picks go on. Returns the indexes kept,
/// in page order: all of them when they already fit, none when every group
/// said none.
async fn narrow(
    cfg: &Config,
    state: &str,
    instructions: &str,
    page: &PageInfo,
    reserved: usize,
    extra: Map<String, Value>,
) -> Result<(Vec<usize>, Option<Value>)> {
    let n = page.elements.len();
    if n + reserved <= CHOICE_LIMIT {
        return Ok(((0..n).collect(), None));
    }
    let groups = even_groups(n, CHOICE_LIMIT - 1);
    let extra = &extra;
    let requests = groups
        .chunks(GROUPS_PER_REQUEST)
        .enumerate()
        .map(|(r, batch)| async move {
            let mut questions = if r == 0 { extra.clone() } else { Map::new() };
            for (g, range) in batch.iter().enumerate() {
                let mut criteria = Map::new();
                for e in &page.elements[range.clone()] {
                    criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
                }
                criteria.insert(
                    "__none__".to_string(),
                    Value::String("None of these elements fits.".to_string()),
                );
                questions.insert(
                    format!("g{g}"),
                    json!({ "type": "choice", "instructions": instructions, "criteria": criteria }),
                );
            }
            let body = json!({ "model": cfg.model, "state": state, "questions": questions });
            let json = post_systemone(cfg, &body).await?;
            let mut picked = Vec::new();
            for (g, range) in batch.iter().enumerate() {
                let mut ids: Vec<&str> = page.elements[range.clone()]
                    .iter()
                    .map(|e| e.id.as_str())
                    .collect();
                ids.push("__none__");
                let answer = parse_choice(&json, &format!("g{g}"), &ids)?;
                let top_two = std::iter::once(answer.chosen.clone())
                    .chain(answer.ranked.into_iter().map(|(id, _)| id))
                    .filter(|id| id != "__none__")
                    .take(2);
                picked.extend(top_two);
            }
            Ok::<_, anyhow::Error>((picked, json))
        });
    let mut first = None;
    let mut picked: Vec<String> = Vec::new();
    for (r, (ids, json)) in futures::future::try_join_all(requests)
        .await?
        .into_iter()
        .enumerate()
    {
        picked.extend(ids);
        if r == 0 {
            first = Some(json);
        }
    }
    let keep = page
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| picked.contains(&e.id))
        .map(|(i, _)| i)
        .collect();
    Ok((keep, first))
}

async fn resolve_with_floor(page: &Page, goal: &str, floor: f64) -> Result<Resolution> {
    let cfg = config()?;
    let page_info = extract_with_retry(page, false).await?;
    if page_info.elements.is_empty() {
        bail!("no interactive elements found on the page — navigate or wait_for content first");
    }
    let checked = page_info.elements.len();
    let started = Instant::now();
    let keep = narrow(
        &cfg,
        &build_state(goal, &page_info.subset(&[])),
        "Which listed element is the best match for what the agent wants to act on? Pick __none__ if none of them is.",
        &page_info,
        1,
        Map::new(),
    )
    .await?
    .0;
    if keep.is_empty() {
        bail!(
            "Jev found nothing matching \"{goal}\" among the page's {checked} elements, so \
             nothing was acted on. take_snapshot and target an explicit CSS selector"
        );
    }
    let page_info = page_info.subset(&keep);

    let state = build_state(goal, &page_info);
    let mut criteria = Map::new();
    for e in &page_info.elements {
        criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
    }
    criteria.insert(
        "__none__".to_string(),
        Value::String("None of these elements is what the agent wants.".to_string()),
    );

    let (element_id, confidence, input_tokens, model) = decide(&cfg, &state, &criteria).await?;
    if element_id == "__none__" {
        bail!(
            "Jev found nothing matching \"{goal}\" among the page's {checked} elements, so \
             nothing was acted on. take_snapshot and target an explicit CSS selector"
        );
    }
    if confidence < floor {
        bail!(
            "Jev's confidence for the best match was only {confidence:.2} (below the \
             {:.2} floor), so nothing was acted on. take_snapshot and target an explicit \
             CSS selector, or phrase the goal more specifically",
            floor
        );
    }
    let description = page_info
        .elements
        .iter()
        .find(|e| e.id == element_id)
        .map(|e| e.desc.clone())
        .unwrap_or_default();
    tracing::debug!(
        model,
        input_tokens,
        confidence,
        elapsed_ms = started.elapsed().as_millis() as u64,
        element = %element_id,
        "jev resolved element"
    );
    Ok(Resolution {
        css: page_info.css(&element_id),
        description,
        confidence,
        offered: checked,
        element_id,
    })
}

/// How many of a step's text fields get their own "which value goes here?"
/// question; a chosen field beyond them is asked about afterwards.
const VALUE_FIELDS: usize = 16;

/// Ways `act` can act on a chosen element besides typing and selecting,
/// asked speculatively in each step's request.
pub const OPERATIONS: &[(&str, &str)] = &[
    (
        "click",
        "Click it. The usual way to use a button, link or control.",
    ),
    (
        "hover",
        "Move the mouse over it without clicking, to reveal hidden content such as a caption, tooltip or menu.",
    ),
    ("right_click", "Right-click it to open its context menu."),
    ("drag", "Drag it and drop it onto another element."),
];

/// Ask Jev where to drop an element `act` is dragging, among the elements
/// the step's extraction offered (except the dragged one). `None` means no
/// target fit well enough.
pub async fn decide_drop_target(
    goal: &str,
    page: &PageInfo,
    dragging: &str,
) -> Result<Option<String>> {
    let cfg = config()?;
    let dragged = page
        .elements
        .iter()
        .find(|e| e.id == dragging)
        .map(|e| e.desc.clone())
        .unwrap_or_default();
    let mut criteria = Map::new();
    for e in page.elements.iter().filter(|e| e.id != dragging) {
        criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
    }
    if criteria.is_empty() {
        return Ok(None);
    }
    let state = format!(
        "Goal: \"{goal}\".\nThe agent is dragging this element: {dragged}\nChoose where to drop it."
    );
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "target": {
                "type": "choice",
                "instructions": "Onto which element should the dragged element be dropped for the goal?",
                "criteria": criteria
            }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "target", &ids)?;
    if answer.confidence < ACT_MIN_CONFIDENCE {
        return Ok(None);
    }
    Ok(Some(answer.chosen))
}

/// Extra actions offered to `act` alongside the elements. Together with
/// [`MAX_ELEMENTS`] they stay under Jev's 255-option Choice limit.
pub const PSEUDO_ACTIONS: &[(&str, &str)] = &[
    (
        "scroll_down",
        "Scroll the page down by most of a viewport to reveal more elements.",
    ),
    ("scroll_up", "Scroll the page up by most of a viewport."),
    (
        "wait",
        "Wait briefly for the page to settle (loading, animation) before deciding again.",
    ),
    ("back", "Go back to the previous page in history."),
    (
        "press_enter",
        "Press Enter in the focused field, e.g. to submit a search just typed.",
    ),
    (
        "press_escape",
        "Press Escape, e.g. to close a popup, menu or dialog.",
    ),
];

/// The `state` string for an `act` step: the goal, the page's identity, what
/// was already done, and the numbered element menu. Recent actions keep the
/// loop from re-picking something that had no effect.
fn build_act_state(
    goal: &str,
    page: &PageInfo,
    recent: &[String],
    value_keys: &[String],
) -> String {
    let mut lines = act_state_head(goal, page, recent, value_keys);
    if page.elements.is_empty() {
        lines.push("(no interactive elements were found on this page)".to_string());
    }
    lines.push("Interactive elements and actions:".to_string());
    lines.extend(
        page.elements
            .iter()
            .map(|e| format!("{}: {}", e.id, e.desc)),
    );
    lines.extend(
        PSEUDO_ACTIONS
            .iter()
            .map(|(id, desc)| format!("{id}: {desc}")),
    );
    if page.omitted > 0 {
        lines.push(format!(
            "({} further elements not listed; scroll if the target may be among them)",
            page.omitted
        ));
    }
    lines.join("\n")
}

/// The part of an `act` step's state before the element menu: goal, page,
/// visible text, steps so far and value keys. On its own it is the state
/// for [`narrow`]'s group questions, whose options carry the elements.
fn act_state_head(
    goal: &str,
    page: &PageInfo,
    recent: &[String],
    value_keys: &[String],
) -> Vec<String> {
    let mut lines = vec![format!("Goal: \"{goal}\".")];
    if page.title.is_empty() {
        lines.push(format!("Page: {}", page.url));
    } else {
        lines.push(format!("Page: {} — {}", page.url, page.title));
    }
    if !page.text.is_empty() {
        lines.push(format!("Visible text: {}", page.text));
    }
    if !recent.is_empty() {
        lines.push("Actions already taken:".to_string());
        lines.extend(recent.iter().map(|r| format!("- {r}")));
    }
    if !value_keys.is_empty() {
        lines.push(format!(
            "Values the caller can type, by key: {}.",
            value_keys.join(", ")
        ));
    }
    lines
}

/// Criteria for an `act` step Choice: every element plus the pseudo-actions.
fn act_criteria(page: &PageInfo) -> Map<String, Value> {
    let mut criteria = Map::new();
    for e in &page.elements {
        criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
    }
    for (id, desc) in PSEUDO_ACTIONS {
        criteria.insert((*id).to_string(), Value::String((*desc).to_string()));
    }
    criteria
}

/// One `act` step decision: what to do next, plus the side judgments that
/// all ride the same fan-out request.
pub struct StepDecision {
    /// The chosen id: an element's `data-jev-id`, or a pseudo-action name.
    /// Empty when the page offered no interactive elements.
    pub chosen: String,
    /// True when `chosen` names an extracted element.
    pub is_element: bool,
    /// The chosen element's description, or the pseudo-action's.
    pub description: String,
    /// Jev's calibrated confidence in the choice.
    pub confidence: f64,
    /// Jev's probability that the goal is already achieved.
    pub goal_met: f64,
    /// Jev's probability that no available action can make progress.
    pub stuck: f64,
    /// Jev's probability that a sign-in wall blocks the goal.
    pub login: f64,
    /// Jev's probability that the page shows an error or rejection.
    pub error: f64,
    /// Jev's probability that the chosen next step can't be undone
    /// (placing an order, paying, sending, deleting).
    pub irreversible: f64,
    /// Jev's probability that the page is a CAPTCHA or bot check.
    pub bot_check: f64,
    /// The top options with their probabilities, chosen first, for an
    /// `uncertain` stop.
    pub candidates: Vec<(String, f64)>,
    /// How to act on a chosen element: one of [`OPERATIONS`]' ids, "click"
    /// unless Jev was sure of another.
    pub operation: String,
    /// Speculative answers from the same request: for each offered text
    /// field (up to [`VALUE_FIELDS`]), the caller-provided value key that
    /// belongs in it, keyed by element id.
    pub field_values: std::collections::HashMap<String, String>,
    /// Next-best element by returned probability, for no-effect recovery.
    pub next_best: Option<(String, f64)>,
    /// Whether the page offered any interactive element at all. When it
    /// didn't, only the Noul judgments are meaningful and the caller
    /// should stop on them or bail.
    pub has_elements: bool,
    /// The extraction this decision was based on — the caller executes
    /// against these stamped elements without re-extracting.
    pub page: PageInfo,
}

impl StepDecision {
    /// How an offered id was described to Jev: an element's description or
    /// a pseudo-action's.
    pub fn describe(&self, id: &str) -> String {
        self.page
            .elements
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.desc.clone())
            .or_else(|| {
                PSEUDO_ACTIONS
                    .iter()
                    .find(|(p, _)| *p == id)
                    .map(|(p, _)| (*p).to_string())
            })
            .unwrap_or_else(|| id.to_string())
    }
}

/// The judgments about the page itself in an `act` step: goal met, a login,
/// error or bot check page. On a page narrowed in groups they are also asked
/// in the group request, beside the page's text alone: next to the
/// goal-picked shortlist, an achieved goal could read as still to do.
fn page_gate_questions() -> Map<String, Value> {
    let mut q = Map::new();
    q.insert(
        "goal_met".to_string(),
        json!({ "type": "noul", "instructions": "The goal has already been fully achieved on the current page." }),
    );
    q.insert(
        "login".to_string(),
        json!({ "type": "noul", "instructions": "A sign-in wall, login form, or authentication prompt is blocking progress toward the goal." }),
    );
    q.insert(
        "error".to_string(),
        json!({ "type": "noul", "instructions": "The page shows an error, rejection, or failure message relevant to the goal." }),
    );
    q.insert(
        "bot_check".to_string(),
        json!({ "type": "noul", "instructions": "The page is a CAPTCHA, a 'verify you are human' check, or another bot challenge instead of the site's content." }),
    );
    q
}

/// Ask Jev for the next `act` step. One fan-out request carries the element
/// Choice, the goal-met / stuck / login / error / irreversible / bot-check
/// Nouls, and, speculatively, the value-to-type Choice: up to eight decisions,
/// one round trip. Extra
/// questions are close to free on this API, so asking the value question
/// before knowing the element saves a whole round trip on form steps.
pub async fn decide_step(
    page: &Page,
    goal: &str,
    recent: &[String],
    value_keys: &[String],
) -> Result<StepDecision> {
    let cfg = config()?;
    let mut page_info = extract_with_retry(page, true).await?;
    let reserved = PSEUDO_ACTIONS.len();
    let mut gates_json = None;
    if page_info.elements.len() + reserved > CHOICE_LIMIT {
        let head = act_state_head(goal, &page_info, recent, value_keys).join("\n");
        let (mut keep, first) = narrow(
            &cfg,
            &head,
            "Which listed element is the best next step toward the goal? Pick __none__ if none of them helps.",
            &page_info,
            reserved,
            page_gate_questions(),
        )
        .await?;
        gates_json = first;
        // Every group said none: offer the page as it reads, dialog and
        // viewport first, and let the step scroll, wait or go back.
        if keep.is_empty() {
            keep = (0..CHOICE_LIMIT - reserved).collect();
        }
        page_info = page_info.subset(&keep);
    }
    let has_elements = !page_info.elements.is_empty();

    let state = build_act_state(goal, &page_info, recent, value_keys);
    let criteria = act_criteria(&page_info);
    let mut questions = Map::new();
    if has_elements {
        questions.insert(
            "element".to_string(),
            json!({
                "type": "choice",
                "instructions": "Which single listed element or action is the best next step toward the goal?",
                "criteria": criteria
            }),
        );
    }
    questions.extend(page_gate_questions());
    questions.insert(
        "stuck".to_string(),
        json!({
            "type": "noul",
            "instructions": "None of the listed elements or actions can make any progress toward the goal."
        }),
    );
    if has_elements {
        let mut ops = Map::new();
        for (id, desc) in OPERATIONS {
            ops.insert((*id).to_string(), Value::String((*desc).to_string()));
        }
        questions.insert(
            "operation".to_string(),
            json!({
                "type": "choice",
                "instructions": "If the best next step acts on one of the listed elements, which way of acting on it does the goal call for?",
                "criteria": ops
            }),
        );
    }
    questions.insert(
        "irreversible".to_string(),
        json!({
            "type": "noul",
            "instructions": "The best next step toward the goal would place an order, make a payment, send a message, publish, or delete something, which cannot be undone."
        }),
    );
    // Which value goes in which field is asked per field, so the answer
    // fits whichever field the step picks ("from" into "Where from?", never
    // "Where to?").
    let mut value_criteria = Map::new();
    let value_fields: Vec<&Element> = if value_keys.is_empty() {
        Vec::new()
    } else {
        page_info
            .elements
            .iter()
            .filter(|e| is_text_field(&e.desc))
            .take(VALUE_FIELDS)
            .collect()
    };
    if !value_fields.is_empty() {
        for k in value_keys {
            value_criteria.insert(
                k.clone(),
                Value::String(format!("the {}", k.replace(['_', '-'], " "))),
            );
        }
        value_criteria.insert(
            "__none__".to_string(),
            Value::String("None of the provided values belongs in this field.".to_string()),
        );
        for e in &value_fields {
            questions.insert(
                format!("value_{}", e.id),
                json!({
                    "type": "choice",
                    "instructions": format!("Which provided value belongs in this field: {}?", e.desc),
                    "criteria": value_criteria
                }),
            );
        }
    }
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": questions
    });
    let json = post_systemone(&cfg, &body).await?;
    let gates = gates_json.as_ref().unwrap_or(&json);
    // Goal met is asked both ways on a narrowed page and the higher answer
    // counts: the page text alone misses a goal named only by a `values` key
    // ("search for the article"), the shortlist beside a goal it just met.
    let goal_met = parse_noul(gates, "goal_met")?.max(parse_noul(&json, "goal_met")?);
    let stuck = parse_noul(&json, "stuck")?;
    let login = parse_noul(gates, "login")?;
    let error = parse_noul(gates, "error")?;
    let irreversible = parse_noul(&json, "irreversible")?;
    let bot_check = parse_noul(gates, "bot_check")?;
    let mut candidates = Vec::new();
    let mut chosen = String::new();
    let mut is_element = false;
    let mut description = String::new();
    let mut confidence = 0.0;
    let mut next_best = None;
    if has_elements {
        let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
        let answer = parse_choice(&json, "element", &ids)?;
        chosen = answer.chosen.clone();
        confidence = answer.confidence;
        is_element = chosen.starts_with('e') && page_info.elements.iter().any(|e| e.id == chosen);
        description = if is_element {
            page_info
                .elements
                .iter()
                .find(|e| e.id == chosen)
                .map(|e| e.desc.clone())
                .unwrap_or_default()
        } else {
            PSEUDO_ACTIONS
                .iter()
                .find(|(id, _)| *id == chosen)
                .map(|(_, desc)| (*desc).to_string())
                .unwrap_or_else(|| chosen.clone())
        };
        candidates.push((chosen.clone(), confidence));
        candidates.extend(answer.ranked.iter().take(2).cloned());
        next_best = answer
            .ranked
            .into_iter()
            .find(|(id, _)| id.starts_with('e') && page_info.elements.iter().any(|e| e.id == *id));
    }
    let mut operation = "click".to_string();
    if has_elements {
        let ids: Vec<&str> = OPERATIONS.iter().map(|(id, _)| *id).collect();
        let answer = parse_choice(&json, "operation", &ids)?;
        if answer.confidence >= ACT_MIN_CONFIDENCE {
            operation = answer.chosen;
        }
    }
    let mut field_values = std::collections::HashMap::new();
    let ids: Vec<&str> = value_criteria.keys().map(|k| k.as_str()).collect();
    for e in &value_fields {
        let answer = parse_choice(&json, &format!("value_{}", e.id), &ids)?;
        if answer.chosen != "__none__" && answer.confidence >= ACT_MIN_CONFIDENCE {
            field_values.insert(e.id.clone(), answer.chosen);
        }
    }
    tracing::debug!(
        model = %json.get("model").and_then(|m| m.as_str()).unwrap_or(""),
        confidence,
        goal_met,
        stuck,
        login,
        error,
        element = %chosen,
        "jev act step decision"
    );
    Ok(StepDecision {
        chosen,
        is_element,
        description,
        confidence,
        goal_met,
        stuck,
        login,
        error,
        irreversible,
        bot_check,
        candidates,
        operation,
        field_values,
        next_best,
        has_elements,
        page: page_info,
    })
}

/// Ask Jev which of the caller's untyped `values` keys belongs in one text
/// field `act` chose. The step's speculative value question is asked before
/// the element is known; this one runs only when that guess came back empty,
/// so it sees the actual field. `None` means no value fits.
pub async fn decide_value_for_field(
    goal: &str,
    field_desc: &str,
    keys: &[String],
) -> Result<Option<String>> {
    let cfg = config()?;
    let state = format!(
        "Goal: \"{goal}\".\nThe agent is about to type into this field: {field_desc}\n\
         Choose which provided value belongs in it."
    );
    let mut criteria = Map::new();
    for k in keys {
        criteria.insert(
            k.clone(),
            Value::String(format!("the {}", k.replace(['_', '-'], " "))),
        );
    }
    criteria.insert(
        "__none__".to_string(),
        Value::String("None of the provided values belongs in this field.".to_string()),
    );
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "value_key": {
                "type": "choice",
                "instructions": "Which provided value belongs in this field?",
                "criteria": criteria
            }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "value_key", &ids)?;
    if answer.chosen == "__none__" || answer.confidence < ACT_MIN_CONFIDENCE {
        return Ok(None);
    }
    Ok(Some(answer.chosen))
}

/// Ask Jev which option of a `<select>` to pick, keyed by live DOM index —
/// labels clamp and can collide, indexes cannot. `None` means leave the
/// field as it is (the caller falls back to a plain click). Returns the
/// chosen option's index and label.
pub async fn decide_select_option(
    goal: &str,
    field_desc: &str,
    options: &[SelectOption],
) -> Result<Option<(u64, String, f64)>> {
    let cfg = config()?;
    let state = format!(
        "Goal: \"{goal}\".\nThe agent is about to set this dropdown: {field_desc}\n\
         Choose the option to select."
    );
    let mut criteria = Map::new();
    for o in options {
        criteria.insert(
            format!("o{}", o.i),
            Value::String(format!("option \"{}\"", o.label)),
        );
    }
    criteria.insert(
        "__none__".to_string(),
        Value::String("None of these options fit; leave the dropdown as it is.".to_string()),
    );
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "element": {
                "type": "choice",
                "instructions": "Which dropdown option best serves the goal?",
                "criteria": criteria
            }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "element", &ids)?;
    if answer.chosen == "__none__" {
        return Ok(None);
    }
    if answer.confidence < ACT_MIN_CONFIDENCE {
        bail!(
            "Jev's confidence for the option was only {:.2} (below the \
             {:.2} floor), so the dropdown was left unchanged",
            answer.confidence,
            ACT_MIN_CONFIDENCE
        );
    }
    let i = answer.chosen[1..]
        .parse::<u64>()
        .map_err(|_| anyhow!("Jev option id `{}` is malformed", answer.chosen))?;
    let label = options
        .iter()
        .find(|o| o.i == i)
        .map(|o| o.label.clone())
        .unwrap_or_default();
    Ok(Some((i, label, answer.confidence)))
}

/// Whether an extracted element description names a field the agent can type
/// into (a text-ish input, a textarea, contenteditable) rather than click.
pub fn is_text_field(desc: &str) -> bool {
    let tag = desc.split_whitespace().next().unwrap_or_default();
    match tag {
        "textarea" => true,
        "input" => {
            let t = desc
                .split("type=")
                .nth(1)
                .and_then(|s| s.split([' ', '"']).next())
                .unwrap_or("text");
            !matches!(
                t,
                "button" | "submit" | "reset" | "image" | "file" | "checkbox" | "radio" | "hidden"
            )
        }
        _ => desc.contains("[contenteditable]"),
    }
}

/// The confidence floor for `act`'s decisions (element, value and option).
/// Lower than `JEV_MIN_CONFIDENCE`'s single picks: on real pages Jev's top
/// pick was usually right at 0.35 to 0.5 when close options split the
/// probability, and `act` checks every action's effect and retries with the
/// next-best pick, which a single `jev:` click can't.
pub const ACT_MIN_CONFIDENCE: f64 = 0.3;

/// Read back the effective model and input-token count from a response.
fn usage_of(body: &Value, cfg: &Config) -> (String, u64) {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(&cfg.model)
        .to_string();
    let input_tokens = body
        .pointer("/usage/input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    (model, input_tokens)
}

/// One Choice call to `/v1/systemone`, retrying what the API asks to retry
/// (408/409/429/5xx, honoring Retry-After) and failing fast on everything
/// else (bad key, oversized request, bad model name).
async fn decide(
    cfg: &Config,
    state: &str,
    criteria: &Map<String, Value>,
) -> Result<(String, f64, u64, String)> {
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "element": {
                "type": "choice",
                "instructions": "Which single listed element is the best match for what the agent wants to act on?",
                "criteria": criteria
            }
        }
    });
    let json = post_systemone(cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "element", &ids)?;
    let (model, input_tokens) = usage_of(&json, cfg);
    Ok((answer.chosen, answer.confidence, input_tokens, model))
}

/// POST a fully-built `/v1/systemone` request and return the parsed response,
/// with the shared retry policy. Building the question set is the caller's
/// job — single Choices and act fan-outs use the same wire path.
async fn post_systemone(cfg: &Config, body: &Value) -> Result<Value> {
    let url = format!("{}/v1/systemone", cfg.base_url.trim_end_matches('/'));
    let mut attempt = 0u32;
    loop {
        let send = CLIENT
            .post(&url)
            .bearer_auth(&cfg.api_key)
            .json(&body)
            .timeout(cfg.timeout)
            .send();
        let resp = match tokio::time::timeout(cfg.timeout, send).await {
            Ok(r) => r.context("requesting a Jev decision")?,
            Err(_) => bail!("Jev request timed out after {}ms", cfg.timeout.as_millis()),
        };
        let status = resp.status();
        if status.is_success() {
            return resp.json().await.context("parsing the Jev API response");
        }
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = resp.text().await.unwrap_or_default();
        let retryable = matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error();
        if !retryable || attempt >= 2 {
            bail!("Jev API returned {status}: {text}");
        }
        attempt += 1;
        let backoff = retry_after
            .filter(|d| d.as_millis() <= 5_000)
            .unwrap_or_else(|| Duration::from_millis(250 * 4u64.pow(attempt)));
        tracing::warn!(
            status = status.as_u16(),
            attempt,
            "Jev API call failed; retrying"
        );
        tokio::time::sleep(backoff).await;
    }
}

fn parse_extraction(v: &Value, stamp: &str) -> Result<PageInfo> {
    let url = v
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let title = v
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let elements = v
        .get("elements")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let id = e.get("id")?.as_str()?.to_string();
                    let desc = e
                        .get("desc")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let options = e
                        .get("options")
                        .and_then(|o| o.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|o| {
                                    Some(SelectOption {
                                        i: o.get("i")?.as_u64()?,
                                        label: o
                                            .get("label")
                                            .and_then(|l| l.as_str())
                                            .unwrap_or_default()
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Element { id, desc, options })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let omitted = v.get("omitted").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let text = v
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if url.is_empty() && elements.is_empty() {
        bail!("element extraction returned no page data (about:blank or a crashed target?)");
    }
    Ok(PageInfo {
        url,
        title,
        text,
        elements,
        omitted,
        stamp: stamp.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_leads_with_goal_and_lists_elements_in_order() {
        let page = PageInfo {
            url: "https://example.com/login".to_string(),
            title: "Example Login".to_string(),
            text: "Email Password Log in".to_string(),
            elements: vec![
                Element {
                    id: "e1".to_string(),
                    desc: "input type=text \"Email\"".to_string(),
                    options: vec![],
                },
                Element {
                    id: "e2".to_string(),
                    desc: "button \"Log in\"".to_string(),
                    options: vec![],
                },
            ],
            omitted: 3,
            stamp: "ab12".to_string(),
        };
        let state = build_state("the login button", &page);
        assert!(state.starts_with("The agent wants to act on: \"the login button\"."));
        assert!(state.contains("Page: https://example.com/login — Example Login"));
        assert!(state.contains("Visible text: Email Password Log in"));
        let i1 = state.find("e1: input type=text \"Email\"").unwrap();
        let i2 = state.find("e2: button \"Log in\"").unwrap();
        assert!(i1 < i2);
        assert!(state.contains("(3 further elements not listed"));
    }

    #[test]
    fn act_state_lists_recent_actions_values_and_pseudo_actions() {
        let page = PageInfo {
            url: "https://example.com/search".to_string(),
            title: "Search".to_string(),
            text: String::new(),
            elements: vec![Element {
                id: "e1".to_string(),
                desc: "input type=search \"Search\"".to_string(),
                options: vec![],
            }],
            omitted: 0,
            stamp: "ab12".to_string(),
        };
        let state = build_act_state(
            "search for oolong tea",
            &page,
            &["typed into the search box".to_string()],
            &["query".to_string()],
        );
        assert!(state.starts_with("Goal: \"search for oolong tea\"."));
        assert!(state.contains("Actions already taken:\n- typed into the search box"));
        assert!(state.contains("Values the caller can type, by key: query."));
        assert!(state.contains("e1: input type=search \"Search\""));
        assert!(state.contains("scroll_down: Scroll the page down"));
        assert!(state.contains("back: Go back to the previous page"));
        assert!(state.contains("wait: Wait briefly"));
    }

    #[test]
    fn act_state_notes_when_no_elements_were_found() {
        let page = PageInfo {
            url: "https://example.com/empty".to_string(),
            title: "Empty".to_string(),
            text: "Nothing here".to_string(),
            elements: vec![],
            omitted: 0,
            stamp: "ab12".to_string(),
        };
        let state = build_act_state("confirm we are on the empty page", &page, &[], &[]);
        assert!(state.contains("(no interactive elements were found on this page)"));
        assert!(state.contains("Visible text: Nothing here"));
    }

    #[test]
    fn act_criteria_stay_under_the_choice_limit() {
        let page = PageInfo {
            url: "https://example.com".to_string(),
            title: String::new(),
            text: String::new(),
            elements: (1..=CHOICE_LIMIT - PSEUDO_ACTIONS.len())
                .map(|i| Element {
                    id: format!("e{i}"),
                    desc: format!("button {i}"),
                    options: vec![],
                })
                .collect(),
            omitted: 0,
            stamp: "ab12".to_string(),
        };
        let criteria = act_criteria(&page);
        assert_eq!(criteria.len(), CHOICE_LIMIT);
        assert!(criteria.contains_key("scroll_down"));
        assert!(criteria.contains_key("back"));
    }

    #[test]
    fn groups_are_even_and_fit_one_choice() {
        let sizes = |n| {
            even_groups(n, CHOICE_LIMIT - 1)
                .iter()
                .map(|r| r.len())
                .collect::<Vec<_>>()
        };
        assert_eq!(sizes(1800), vec![225; 8]);
        assert_eq!(sizes(254), vec![254]);
        assert_eq!(sizes(255), vec![128, 127]);
        assert_eq!(sizes(0), Vec::<usize>::new());
        let g = even_groups(1801, CHOICE_LIMIT - 1);
        assert_eq!(g.first().unwrap().start, 0);
        assert_eq!(g.last().unwrap().end, 1801);
        assert!(g.iter().all(|r| r.len() < CHOICE_LIMIT));
    }

    #[test]
    fn answer_must_name_an_offered_element() {
        let body = json!({
            "answers": { "element": { "choice": "e2", "confidence": 0.93 } },
            "model": "jev-1.14.0",
            "usage": { "input_tokens": 1712 }
        });
        let a = parse_choice(&body, "element", &["e1", "e2"]).unwrap();
        assert_eq!(a.chosen, "e2");
        assert!((a.confidence - 0.93).abs() < f64::EPSILON);
        assert!(a.ranked.is_empty());

        let bogus = json!({ "answers": { "element": { "choice": "e99", "confidence": 1.0 } } });
        assert!(parse_choice(&bogus, "element", &["e1", "e2"]).is_err());

        let missing = json!({ "answers": {} });
        assert!(parse_choice(&missing, "element", &["e1", "e2"]).is_err());
    }

    #[test]
    fn noul_answers_are_read_per_question() {
        let body = json!({
            "answers": {
                "goal_met": { "noul": 0.91 },
                "stuck": { "noul": 0.03 }
            }
        });
        assert!((parse_noul(&body, "goal_met").unwrap() - 0.91).abs() < f64::EPSILON);
        assert!((parse_noul(&body, "stuck").unwrap() - 0.03).abs() < f64::EPSILON);
        assert!(parse_noul(&body, "missing").is_err());
    }

    #[test]
    fn confidence_falls_back_to_the_chosen_probability() {
        let body = json!({
            "answers": {
                "element": {
                    "choice": "e1",
                    "probabilities": { "e1": 0.7, "e2": 0.3 }
                }
            }
        });
        let a = parse_choice(&body, "element", &["e1", "e2"]).unwrap();
        assert!((a.confidence - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn probabilities_are_validated_and_ranked() {
        // Well-formed: full distribution, chosen is the argmax, the rest
        // comes back ranked for next-best-option recovery.
        let body = json!({
            "answers": {
                "element": {
                    "choice": "e1",
                    "confidence": 0.8,
                    "probabilities": { "e1": 0.8, "e2": 0.15, "e3": 0.05 }
                }
            }
        });
        let a = parse_choice(&body, "element", &["e1", "e2", "e3"]).unwrap();
        assert_eq!(a.ranked.len(), 2);
        assert_eq!(a.ranked[0].0, "e2");
        assert!((a.ranked[0].1 - 0.15).abs() < f64::EPSILON);
        assert_eq!(a.ranked[1].0, "e3");

        // A probability for an id that was never offered is rejected.
        let ghost = json!({
            "answers": {
                "element": {
                    "choice": "e1",
                    "probabilities": { "e1": 0.5, "e9": 0.5 }
                }
            }
        });
        assert!(parse_choice(&ghost, "element", &["e1", "e2"]).is_err());

        // A distribution that doesn't sum to ~1 is rejected.
        let lopsided = json!({
            "answers": {
                "element": {
                    "choice": "e1",
                    "probabilities": { "e1": 0.5, "e2": 0.1 }
                }
            }
        });
        assert!(parse_choice(&lopsided, "element", &["e1", "e2"]).is_err());

        // Chosen must be the argmax of its own probabilities.
        let contradicting = json!({
            "answers": {
                "element": {
                    "choice": "e1",
                    "probabilities": { "e1": 0.2, "e2": 0.8 }
                }
            }
        });
        assert!(parse_choice(&contradicting, "element", &["e1", "e2"]).is_err());
    }

    #[test]
    fn text_fields_are_recognized_from_descriptions() {
        assert!(is_text_field("input type=search \"Search\""));
        assert!(is_text_field("input #card (filled)"));
        assert!(is_text_field("input #em type=email \"Email\" (empty)"));
        assert!(is_text_field("input #email \"Email\"")); // no type= → text
        assert!(is_text_field("input type=password (value hidden)"));
        assert!(is_text_field("textarea #bio"));
        assert!(is_text_field(
            "div #editor [contenteditable] \"Write here\""
        ));
        assert!(!is_text_field("input type=submit \"Log in\""));
        assert!(!is_text_field("input type=checkbox \"Accept\""));
        assert!(!is_text_field("button \"Go\""));
        assert!(!is_text_field("a \"Home\" -> /"));
    }

    #[test]
    fn extraction_parses_ids_descriptions_and_omissions() {
        let raw = json!({
            "url": "https://example.com",
            "title": "Example",
            "text": "Example page text",
            "elements": [
                { "id": "e1", "desc": "a \"More information...\" -> https://example.com" },
                { "id": "e2", "desc": "select #color selected=\"Red\"", "options": [
                    { "i": 0, "label": "Red" },
                    { "i": 1, "label": "Green" },
                    { "i": 2, "label": "Blue" }
                ] },
                { "id": "e3", "desc": "input type=password (value hidden)" }
            ],
            "omitted": 12
        });
        let page = parse_extraction(&raw, "ab12").unwrap();
        assert_eq!(page.elements.len(), 3);
        assert_eq!(page.elements[0].id, "e1");
        assert_eq!(page.elements[1].options.len(), 3);
        assert_eq!(page.elements[1].options[1].i, 1);
        assert_eq!(page.elements[1].options[1].label, "Green");
        assert!(page.elements[2].options.is_empty());
        assert_eq!(page.omitted, 12);
        assert_eq!(page.text, "Example page text");
        assert!(parse_extraction(&json!({}), "ab12").is_err());
        assert_eq!(page.css("e2"), "[data-jev-id=\"ab12-e2\"]");
    }
}
