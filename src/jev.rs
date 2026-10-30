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
//!   3. act on [data-jev-id="e17"] through the same trusted-CDP path as ever
//! ```
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
/// the template below so the Rust and JS caps can't drift apart.
const EXTRACT_FN_TEMPLATE: &str = r#"function() {
  var MAX = __MAX_ELEMENTS__;
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
    if (el.type === 'password') {
      p.push('(value hidden)');
    } else {
      var label = el.getAttribute('aria-label') || el.getAttribute('placeholder') ||
        el.getAttribute('title') ||
        (el.labels && el.labels[0] ? el.labels[0].innerText : '') ||
        el.innerText || el.value || el.getAttribute('value') || '';
      if (label) p.push('"' + clamp(label, 80) + '"');
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
    chosen[j].setAttribute('data-jev-id', id);
    var rec = { id: id, desc: describe(chosen[j]) };
    if (chosen[j].tagName === 'SELECT' && chosen[j].options) {
      var opts = [];
      for (var q = 0; q < chosen[j].options.length && q < 25; q++) {
        opts.push(clamp(chosen[j].options[q].label, 60));
      }
      rec.options = opts;
    }
    elements.push(rec);
  }
  return {
    url: String(location.href),
    title: clamp(document.title, 120),
    elements: elements,
    omitted: Math.max(0, inView.length + offView.length - chosen.length)
  };
}"#;

/// The extraction script, with [`MAX_ELEMENTS`] baked in.
static EXTRACT_FN: Lazy<String> =
    Lazy::new(|| EXTRACT_FN_TEMPLATE.replace("__MAX_ELEMENTS__", &MAX_ELEMENTS.to_string()));

/// One resolved `jev:` goal: everything the caller needs to act and to tell
/// the agent what was picked.
pub struct Resolution {
    /// The chosen element's `data-jev-id` value, e.g. "e17".
    pub element_id: String,
    /// CSS selector targeting it, e.g. `[data-jev-id="e17"]`.
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

/// One extracted interactive element.
pub struct Element {
    /// Its `data-jev-id` value, e.g. "e17".
    pub id: String,
    /// Compact description, as offered to Jev.
    pub desc: String,
    /// For `<select>` elements: the option labels (capped), in order.
    pub options: Vec<String>,
}

/// A page extraction: what a Jev decision was offered.
pub struct PageInfo {
    pub url: String,
    pub title: String,
    /// Elements in offer order.
    pub elements: Vec<Element>,
    /// Interactive elements beyond [`MAX_ELEMENTS`], not offered.
    pub omitted: usize,
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

/// Pull a Choice answer for `question` out of a `/v1/systemone` response and
/// validate it against the options actually offered. An answer naming anything
/// else — or a response without one — is an error, never a guess.
fn parse_choice(body: &Value, question: &str, valid: &[&str]) -> Result<(String, f64)> {
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
    Ok((chosen.to_string(), confidence))
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
    for attempt in 1..=ATTEMPTS {
        match iso_eval_function(page, EXTRACT_FN.as_str()).await {
            Ok(raw) => match parse_extraction(&raw) {
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
        css: format!("[data-jev-id=\"{element_id}\"]"),
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

/// One `act` step decision: what to do next, plus the goal/stuck judgments.
pub struct StepDecision {
    /// The chosen id: an element's `data-jev-id`, or a pseudo-action name.
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
    /// The extraction this decision was based on — the caller executes
    /// against these stamped elements without re-extracting.
    pub page: PageInfo,
}

/// Ask Jev for the next `act` step. One fan-out request carries the element
/// Choice plus goal-met and stuck Nouls — three decisions, one round trip.
pub async fn decide_step(
    page: &Page,
    goal: &str,
    recent: &[String],
    value_keys: &[String],
) -> Result<StepDecision> {
    let cfg = config()?;
    let page_info = extract_with_retry(page).await?;
    if page_info.elements.is_empty() {
        bail!("no interactive elements found on the page — navigate or wait_for content first");
    }

    let state = build_act_state(goal, &page_info, recent, value_keys);
    let criteria = act_criteria(&page_info);
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "element": {
                "type": "choice",
                "instructions": "Which single listed element or action is the best next step toward the goal?",
                "criteria": criteria
            },
            "goal_met": {
                "type": "noul",
                "instructions": "The goal has already been fully achieved on the current page."
            },
            "stuck": {
                "type": "noul",
                "instructions": "None of the listed elements or actions can make any progress toward the goal."
            }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let (chosen, confidence) = parse_choice(&json, "element", &ids)?;
    let goal_met = parse_noul(&json, "goal_met")?;
    let stuck = parse_noul(&json, "stuck")?;
    let is_element = chosen.starts_with('e') && page_info.elements.iter().any(|e| e.id == chosen);
    let description = if is_element {
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
    tracing::debug!(
        model = %json.get("model").and_then(|m| m.as_str()).unwrap_or(""),
        confidence,
        goal_met,
        stuck,
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
        page: page_info,
    })
}

/// Ask Jev which caller-provided value belongs in the chosen field. `None`
/// means "none of them — don't type here". Only the keys are ever sent; the
/// values themselves go straight from the caller to CDP.
pub async fn decide_value_key(
    goal: &str,
    field_desc: &str,
    keys: &[String],
) -> Result<Option<(String, f64)>> {
    let cfg = config()?;
    let mut state = format!(
        "Goal: \"{goal}\".\nThe agent is about to interact with this field: {field_desc}\n\
         The caller provided values to type, keyed by what they are:"
    );
    state.push('\n');
    state.push_str(
        &keys
            .iter()
            .map(|k| format!("- {k}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let mut criteria = Map::new();
    for k in keys {
        criteria.insert(
            k.clone(),
            Value::String(format!("the provided value keyed \"{k}\"")),
        );
    }
    criteria.insert(
        "__none__".to_string(),
        Value::String(
            "None of these values belong in this field; just click it instead.".to_string(),
        ),
    );
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "element": {
                "type": "choice",
                "instructions": "Which provided value belongs in this field?",
                "criteria": criteria
            }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let (chosen, confidence) = parse_choice(&json, "element", &ids)?;
    if chosen == "__none__" {
        return Ok(None);
    }
    if confidence < cfg.min_confidence {
        bail!(
            "Jev's confidence for the value choice was only {confidence:.2} (below the \
             {:.2} floor), so nothing was typed",
            cfg.min_confidence
        );
    }
    Ok(Some((chosen, confidence)))
}

/// Ask Jev which option of a `<select>` to pick. `None` means leave the field
/// as it is (the caller falls back to a plain click).
pub async fn decide_select_option(
    goal: &str,
    field_desc: &str,
    options: &[String],
) -> Result<Option<(String, f64)>> {
    let cfg = config()?;
    let state = format!(
        "Goal: \"{goal}\".\nThe agent is about to set this dropdown: {field_desc}\n\
         Choose the option to select."
    );
    let mut criteria = Map::new();
    for o in options {
        criteria.insert(o.clone(), Value::String(format!("option \"{o}\"")));
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
    let (chosen, confidence) = parse_choice(&json, "element", &ids)?;
    if chosen == "__none__" {
        return Ok(None);
    }
    if confidence < cfg.min_confidence {
        bail!(
            "Jev's confidence for the option was only {confidence:.2} (below the \
             {:.2} floor), so the dropdown was left unchanged",
            cfg.min_confidence
        );
    }
    Ok(Some((chosen, confidence)))
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
    let (chosen, confidence) = parse_choice(&json, "element", &ids)?;
    let (model, input_tokens) = usage_of(&json, cfg);
    Ok((chosen, confidence, input_tokens, model))
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

fn parse_extraction(v: &Value) -> Result<PageInfo> {
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
                                .filter_map(|o| o.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Element { id, desc, options })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let omitted = v.get("omitted").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    if url.is_empty() && elements.is_empty() {
        bail!("element extraction returned no page data (about:blank or a crashed target?)");
    }
    Ok(PageInfo {
        url,
        title,
        elements,
        omitted,
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
        };
        let state = build_state("the login button", &page);
        assert!(state.starts_with("The agent wants to act on: \"the login button\"."));
        assert!(state.contains("Page: https://example.com/login — Example Login"));
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
            elements: vec![Element {
                id: "e1".to_string(),
                desc: "input type=search \"Search\"".to_string(),
                options: vec![],
            }],
            omitted: 0,
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
        assert!(state.contains("wait: Wait briefly"));
    }

    #[test]
    fn act_criteria_stay_under_the_choice_limit() {
        let page = PageInfo {
            url: "https://example.com".to_string(),
            title: String::new(),
            elements: (1..=MAX_ELEMENTS)
                .map(|i| Element {
                    id: format!("e{i}"),
                    desc: format!("button {i}"),
                    options: vec![],
                })
                .collect(),
            omitted: 0,
        };
        let criteria = act_criteria(&page);
        assert_eq!(criteria.len(), MAX_ELEMENTS + PSEUDO_ACTIONS.len());
        assert!(criteria.len() <= 255);
        assert!(criteria.contains_key("scroll_down"));
    }

    #[test]
    fn answer_must_name_an_offered_element() {
        let body = json!({
            "answers": { "element": { "choice": "e2", "confidence": 0.93 } },
            "model": "jev-1.14.0",
            "usage": { "input_tokens": 1712 }
        });
        let (chosen, confidence) = parse_choice(&body, "element", &["e1", "e2"]).unwrap();
        assert_eq!(chosen, "e2");
        assert!((confidence - 0.93).abs() < f64::EPSILON);

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
        let (_, confidence) = parse_choice(&body, "element", &["e1", "e2"]).unwrap();
        assert!((confidence - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn text_fields_are_recognized_from_descriptions() {
        assert!(is_text_field("input type=search \"Search\""));
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
            "elements": [
                { "id": "e1", "desc": "a \"More information...\" -> https://example.com" },
                { "id": "e2", "desc": "select #color selected=\"Red\"", "options": ["Red", "Green", "Blue"] },
                { "id": "e3", "desc": "input type=password (value hidden)" }
            ],
            "omitted": 12
        });
        let page = parse_extraction(&raw).unwrap();
        assert_eq!(page.elements.len(), 3);
        assert_eq!(page.elements[0].id, "e1");
        assert_eq!(page.elements[1].options, vec!["Red", "Green", "Blue"]);
        assert!(page.elements[2].options.is_empty());
        assert_eq!(page.omitted, 12);
        assert!(parse_extraction(&json!({})).is_err());
    }
}
