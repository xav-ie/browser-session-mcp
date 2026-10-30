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

/// How many interactive elements to offer Jev. Below the 255-option Choice
/// limit with headroom for the criteria map.
const MAX_ELEMENTS: usize = 240;

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
  var SEL = 'a[href], button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="option"], [role="checkbox"], [role="radio"], [role="switch"], [onclick]';
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
  var stale = document.querySelectorAll('[data-jev-id]');
  for (var k = 0; k < stale.length; k++) stale[k].removeAttribute('data-jev-id');
  var vw = document.documentElement.clientWidth || 0;
  var vh = document.documentElement.clientHeight || 0;
  var inView = [], offView = [];
  var nodes = document.querySelectorAll(SEL);
  for (var i = 0; i < nodes.length; i++) {
    var el = nodes[i];
    if (el.disabled) continue;
    if (!el.getClientRects().length) continue;
    var st = getComputedStyle(el);
    if (st.visibility === 'hidden' || st.display === 'none') continue;
    var r = el.getBoundingClientRect();
    var visible = r.bottom > -20 && r.top < vh + 20 && r.right > -20 && r.left < vw + 20;
    (visible ? inView : offView).push(el);
  }
  var chosen = inView.concat(offView).slice(0, MAX);
  var elements = [];
  for (var j = 0; j < chosen.length; j++) {
    var id = 'e' + (j + 1);
    chosen[j].setAttribute('data-jev-id', STAMP + '-' + id);
    var rec = { id: id, desc: describe(chosen[j]) };
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
  var modal = document.querySelector('dialog[open], [role="dialog"], [aria-modal="true"]');
  var text = '';
  if (modal && modal.getClientRects().length) text = textOf(modal);
  else if (document.body) text = textOf(document.body);
  return {
    url: String(location.href),
    title: clamp(document.title, 120),
    text: text,
    elements: elements,
    omitted: Math.max(0, inView.length + offView.length - chosen.length)
  };
}"#;

/// The extraction script, with [`MAX_ELEMENTS`] baked in.
static EXTRACT_FN: Lazy<String> =
    Lazy::new(|| EXTRACT_FN_TEMPLATE.replace("__MAX_ELEMENTS__", &MAX_ELEMENTS.to_string()));

/// The extraction script for one call, stamping with `stamp` (hex only, so
/// it is safe inside the JS string literal).
fn extract_fn(stamp: &str) -> String {
    EXTRACT_FN.replace("__STAMP__", stamp)
}

/// A fresh random stamp token for one extraction.
fn new_stamp() -> Result<String> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

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
pub struct SelectOption {
    pub i: u64,
    pub label: String,
}

/// One extracted interactive element.
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
    lines.push("Interactive elements:".to_string());
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
            if let Some((best, _)) = &top {
                if best.as_str() != chosen {
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
async fn extract_with_retry(page: &Page) -> Result<PageInfo> {
    const ATTEMPTS: usize = 3;
    const PAUSE_MS: u64 = 400;
    let mut last: Option<anyhow::Error> = None;
    let stamp = new_stamp()?;
    let script = extract_fn(&stamp);
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

async fn resolve_with_floor(page: &Page, goal: &str, floor: f64) -> Result<Resolution> {
    let cfg = config()?;
    let page_info = extract_with_retry(page).await?;
    if page_info.elements.is_empty() {
        bail!("no interactive elements found on the page — navigate or wait_for content first");
    }

    let state = build_state(goal, &page_info);
    let mut criteria = Map::new();
    for e in &page_info.elements {
        criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
    }

    let started = Instant::now();
    let (element_id, confidence, input_tokens, model) = decide(&cfg, &state, &criteria).await?;
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
        offered: page_info.elements.len(),
        element_id,
    })
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
    /// Speculative answer from the same request: which caller-provided
    /// value belongs in the field to fill next, if any.
    pub value_key: Option<String>,
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

/// Ask Jev for the next `act` step. One fan-out request carries the element
/// Choice, the goal-met / stuck / login / error Nouls, and — speculatively —
/// the value-to-type Choice: up to six decisions, one round trip. Extra
/// questions are close to free on this API, so asking the value question
/// before knowing the element saves a whole round trip on form steps.
pub async fn decide_step(
    page: &Page,
    goal: &str,
    recent: &[String],
    value_keys: &[String],
) -> Result<StepDecision> {
    let cfg = config()?;
    let page_info = extract_with_retry(page).await?;
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
    questions.insert(
        "goal_met".to_string(),
        json!({
            "type": "noul",
            "instructions": "The goal has already been fully achieved on the current page."
        }),
    );
    questions.insert(
        "stuck".to_string(),
        json!({
            "type": "noul",
            "instructions": "None of the listed elements or actions can make any progress toward the goal."
        }),
    );
    questions.insert(
        "login".to_string(),
        json!({
            "type": "noul",
            "instructions": "A sign-in wall, login form, or authentication prompt is blocking progress toward the goal."
        }),
    );
    questions.insert(
        "error".to_string(),
        json!({
            "type": "noul",
            "instructions": "The page shows an error, rejection, or failure message relevant to the goal."
        }),
    );
    let mut value_criteria = Map::new();
    if has_elements && !value_keys.is_empty() {
        for k in value_keys {
            value_criteria.insert(k.clone(), Value::String(format!("the value for {k}")));
        }
        value_criteria.insert(
            "__none__".to_string(),
            Value::String("No typing is needed in the next step.".to_string()),
        );
        questions.insert(
            "value_key".to_string(),
            json!({
                "type": "choice",
                "instructions": "If the next step is to type into a field, which provided value belongs there? Otherwise pick __none__.",
                "criteria": value_criteria
            }),
        );
    }
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": questions
    });
    let json = post_systemone(&cfg, &body).await?;
    let goal_met = parse_noul(&json, "goal_met")?;
    let stuck = parse_noul(&json, "stuck")?;
    let login = parse_noul(&json, "login")?;
    let error = parse_noul(&json, "error")?;
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
        next_best = answer
            .ranked
            .into_iter()
            .find(|(id, _)| id.starts_with('e') && page_info.elements.iter().any(|e| e.id == *id));
    }
    let mut value_key = None;
    if !value_criteria.is_empty() {
        let ids: Vec<&str> = value_criteria.keys().map(|k| k.as_str()).collect();
        let answer = parse_choice(&json, "value_key", &ids)?;
        if answer.chosen != "__none__" && answer.confidence >= cfg.min_confidence {
            value_key = Some(answer.chosen);
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
        value_key,
        next_best,
        has_elements,
        page: page_info,
    })
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
    if answer.confidence < cfg.min_confidence {
        bail!(
            "Jev's confidence for the option was only {:.2} (below the \
             {:.2} floor), so the dropdown was left unchanged",
            answer.confidence,
            cfg.min_confidence
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

/// The floor applied to `act` element choices (the explicit-goal,
/// best-guess `JEV_MIN_CONFIDENCE` — `act` is only called on purpose).
pub fn min_confidence() -> f64 {
    env_f64("JEV_MIN_CONFIDENCE", 0.5).clamp(0.0, 1.0)
}

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
            elements: (1..=MAX_ELEMENTS)
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
        assert_eq!(criteria.len(), MAX_ELEMENTS + PSEUDO_ACTIONS.len());
        assert!(criteria.len() <= 255);
        assert!(criteria.contains_key("scroll_down"));
        assert!(criteria.contains_key("back"));
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
