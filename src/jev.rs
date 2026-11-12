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
//!   1. extract the page's interactive elements from the DOM (dialog and
//!      viewport first), keeping a reference to each in the isolated world
//!   2. one Choice call over their ids, goal in the state string; a page
//!      with more than fit one Choice is narrowed in even groups first
//!   3. act on the referenced element through the same trusted-CDP path
//! ```
//!
//! The references live in the isolated world, keyed by a random token per
//! extraction, so nothing is written to the page for its scripts to notice,
//! and an answer from an earlier extraction (another `act`, another MCP
//! process on the same tab) still lands on exactly the element it was
//! offered, never on whatever another menu called "e17".
//!
//! Everything fails closed: no key configured, empty page, low confidence,
//! malformed answer — the tool errors and the caller falls back to a CSS
//! selector. A Jev answer never turns into coordinates or JS on its own; it
//! only selects among ids the extraction just offered.
use anyhow::{Context, Result, anyhow, bail};
use chromiumoxide::Page;
use once_cell::sync::Lazy;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

use crate::server::{iso_eval_function, iso_eval_top};

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
/// DOM). Offers them dialog first, then the viewport, then the rest, and
/// keeps references to them in the isolated world under the call's token
/// (see [`PageInfo::css`]). [`MAX_ELEMENTS`] is injected into the template
/// below so the Rust and JS caps can't drift apart; the token is injected
/// per call (see [`extract_fn`]).
const EXTRACT_FN_TEMPLATE: &str = r#"function() {
  __ICON_HINT__
  var MAX = __MAX_ELEMENTS__;
  var STAMP = '__STAMP__';
  var SKIP_TYPED = __SKIP_TYPED__;
  var SEL = 'a[href], button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="option"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [role="slider"], [onclick], [oncontextmenu], [ondblclick], [draggable="true"], [tabindex]:not([tabindex="-1"])';
  var clamp = function(s, n) {
    s = String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
    var a = Array.from(s);
    return a.length > n ? a.slice(0, Math.max(0, n - 1)).join('') + '...' : s;
  };
  var FIELDS = 'input:not([type=hidden]), select, textarea, [role=combobox], [role=listbox], [contenteditable="true"]';
  var near = function(el) {
    var n = el;
    for (var up = 0; up < 4 && n.parentElement; up++) {
      n = n.parentElement;
      var others = n.querySelectorAll(FIELDS);
      for (var o = 0; o < others.length; o++) {
        if (others[o] !== el && !el.contains(others[o]) && !others[o].contains(el)) return '';
      }
      var parts = [];
      var tw = (n.ownerDocument || document).createTreeWalker(n, NodeFilter.SHOW_TEXT);
      for (var t = tw.nextNode(); t; t = tw.nextNode()) {
        var par = t.parentElement;
        if (el.contains(t) || !par || par.closest('select, option, script, style, [role=listbox]')) continue;
        if (t.textContent.trim()) parts.push(t.textContent.trim());
      }
      var text = parts.join(' ').trim();
      if (text) return text;
    }
    return '';
  };
  // The question a radio or checkbox answers: its fieldset's legend, its
  // group's name, or the text around the group that isn't an option's own.
  var groupQuestion = function(el) {
    var named = function(g) {
      if (!g) return '';
      var lb = String(g.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
        .map(function(id) { var n = g.ownerDocument.getElementById(id); return n ? n.textContent : ''; }).join(' ');
      return String(g.getAttribute('aria-label') || lb || '').replace(/\s+/g, ' ').trim();
    };
    var fs = el.closest('fieldset');
    var legend = fs && fs.querySelector('legend');
    if (legend && String(legend.innerText || '').trim()) return String(legend.innerText).replace(/\s+/g, ' ').trim();
    var rg = named(el.closest('[role=radiogroup], [role=group]')) || named(fs);
    if (rg) return rg;
    var mine = function(n) {
      return n.type === el.type && (el.name ? n.name === el.name : true) ||
        (n.getAttribute && /^(radio|checkbox)$/.test(n.getAttribute('role') || ''));
    };
    var n = el;
    for (var up = 0; up < 5 && n.parentElement; up++) {
      n = n.parentElement;
      var fields = n.querySelectorAll(FIELDS + ', [role=radio], [role=checkbox]');
      for (var o = 0; o < fields.length; o++) if (!mine(fields[o])) return '';
      var parts = [];
      var tw = (n.ownerDocument || document).createTreeWalker(n, NodeFilter.SHOW_TEXT);
      for (var t = tw.nextNode(); t; t = tw.nextNode()) {
        var par = t.parentElement;
        if (!par || par.closest('label, script, style, [role=radio], [role=checkbox]')) continue;
        // Bare text right after an option is that option's label.
        var prev = t.previousSibling;
        while (prev && prev.nodeType === 3 && !prev.textContent.trim()) prev = prev.previousSibling;
        if (prev && prev.nodeType === 1 && prev.matches && prev.matches('input[type=radio], input[type=checkbox]')) continue;
        if (t.textContent.trim()) parts.push(t.textContent.trim());
      }
      var text = parts.join(' ').trim();
      if (text) return text;
    }
    return '';
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
      if (el.type === 'file') p.push('(file upload)');
    }
    // A checkbox or radio's `value` is form data ("on" by default), not a
    // label; unlabeled ones often sit next to bare text instead
    // (`<input type=checkbox> checkbox 1`).
    var toggle = tag === 'input' && (el.type === 'checkbox' || el.type === 'radio');
    // The text a person sees on it. A web component's button often holds
    // only a <slot>, its text living in the light DOM outside it, so
    // innerText is empty (an admin UI built of web components); the slotted and
    // shadow-root text is read instead.
    var shown = function() {
      var t = String(el.innerText || '');
      if (t.trim()) return t;
      var parts = [], size = 0;
      var walk = function(n, d) {
        if (d > 8 || size > 160) return;
        if (n.nodeType === 3) { parts.push(n.textContent); size += n.textContent.length; return; }
        if (n.nodeType !== 1 || /^(STYLE|SCRIPT|TEMPLATE)$/.test(n.tagName)) return;
        var kids = n.tagName === 'SLOT' ? n.assignedNodes({ flatten: true }) :
          n.shadowRoot ? n.shadowRoot.childNodes : n.childNodes;
        for (var k = 0; k < kids.length; k++) walk(kids[k], d + 1);
      };
      walk(el, 0);
      return parts.join(' ');
    };
    var labelledBy = function(attr) {
      var ids = String(el.getAttribute(attr || 'aria-labelledby') || '').split(/\s+/).filter(Boolean);
      var root = el.getRootNode();
      return ids.map(function(id) {
        var n = root.getElementById ? root.getElementById(id) : document.getElementById(id);
        return n ? n.textContent : '';
      }).join(' ').trim();
    };
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
      var name = el.getAttribute('aria-label') || labelledBy() || el.getAttribute('placeholder') ||
        el.getAttribute('title') ||
        (el.labels && el.labels[0] ? el.labels[0].innerText : '');
      if (name) p.push('"' + clamp(name, 80) + '"');
    } else {
      var label = el.getAttribute('aria-label') || labelledBy() || el.getAttribute('placeholder') ||
        el.getAttribute('title') ||
        (el.labels && el.labels[0] ? el.labels[0].innerText : '') ||
        shown().trim() || (toggle ? adjacent() : el.value || el.getAttribute('value')) || '';
      if (label) p.push('"' + clamp(label, 80) + '"');
      // An icon with no name, or only an icon font's glyph for one: what
      // its markup says it shows.
      if (!/[\p{L}\p{N}]/u.test(label)) {
        var icon = __bsIcon(el);
        if (icon) p.push('(icon "' + clamp(icon, 40) + '")');
      }
    }
    // A field whose own label is missing or only a placeholder: the
    // question it answers usually sits beside it in a wrapper that holds no
    // other field (a heading div, or a <label> whose `for` names no id).
    // The question a field answers, where the page names it as a
    // description ("Resume" for a "Choose file" button).
    var described = labelledBy('aria-describedby');
    if (described) p.push('described as "' + clamp(described, 80) + '"');
    if (dropZone.has(el) && !fileFor.has(el)) {
      var dq = near(el);
      return p.join(' ') + ' (drop zone' + (dq ? ' under "' + clamp(dq, 100) + '"' : '') + ')';
    }
    if (fileFor.has(el)) {
      var fq = near(el);
      return p.join(' ') + ' (file upload' + (fq ? ' under "' + clamp(fq, 100) + '"' : '') + ')';
    }
    var own = el.getAttribute('aria-label') || labelledBy() ||
      Array.prototype.some.call(el.labels || [], function(l) { return String(l.innerText || '').trim(); });
    var field = typed || tag === 'select' || el.type === 'file' || role === 'combobox' || role === 'listbox';
    if (field && !own) {
      var q = near(el);
      if (q) p.push('under "' + clamp(q, 100) + '"');
    }
    if (typed || el.type === 'password') {
      var content = el.isContentEditable ? el.innerText : el.value;
      p.push(String(content || '').trim() ? '(filled)' : '(empty)');
    }
    // Current state, so a goal like "check the box" can read as met and a
    // toggle isn't clicked again (and undone) on the next step.
    if (toggle) p.push(el.checked ? '(checked)' : '(unchecked)');
    if (toggle || role === 'radio' || role === 'checkbox') {
      var gq = groupQuestion(el);
      if (gq) p.push('under "' + clamp(gq, 100) + '"');
    }
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
  var fileFor = new WeakMap(), dropZone = new WeakSet(), replaced = new WeakSet();
  var consider = function(el, ox, oy) {
    if (seen.has(el) || el.disabled) return;
    seen.add(el);
    // A field this act run already filled stays out of the offer while it
    // holds text, so the loop moves on instead of re-picking it.
    if (SKIP_TYPED && globalThis.__bsTyped && globalThis.__bsTyped.has(el) && String(el.value || el.innerText || '').trim()) return;
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
    var nodes = R.root.querySelectorAll(SEL);
    for (var i = 0; i < nodes.length; i++) consider(nodes[i], R.ox, R.oy);
    // A file input is usually hidden behind a drop zone or button; the box
    // a person sees around it stands in for it.
    // A folder input beside a file one is the same picker: only the file one counts.
    var files = R.root.querySelectorAll('input[type=file]:not([webkitdirectory])');
    for (var fi = 0; fi < files.length; fi++) {
      var f = files[fi];
      if (f.disabled) continue;
      // Shown means a person can see it: a see-through or sliver-thin input
      // laid over a widget (FilePond's) is hidden as far as they can tell.
      var fr = f.getBoundingClientRect(), fs = styleOf(f);
      var faint = parseFloat(fs.opacity) < 0.1 || fr.width < 10 || fr.height < 10;
      if (f.getClientRects().length && fs.visibility !== 'hidden' && !faint) continue;
      var box = f.parentElement;
      for (var fu = 0; box && fu < 4 && !box.getClientRects().length; fu++) box = box.parentElement;
      // An input parked in the body (as some libraries do) has no box of its own.
      if (!box || /^(BODY|HTML)$/.test(box.tagName) || box.querySelectorAll('input[type=file]:not([webkitdirectory])').length > 1) continue;
      fileFor.set(box, f);
      replaced.add(f);
      seen.delete(box);
      consider(box, R.ox, R.oy);
    }
    // Drop zones a script listens on, with no file input to find them by:
    // the innermost box that says so in its class, id or own text.
    // Words telling a person to drop something, not prose about dragging.
    var DROP_TEXT = /drag (and|&|'?n'?) drop( (your |a |the )?([\w-]+ ){0,2})?(here|files?)\b|\bdrop (your |the |a |an )?([\w-]+ ){0,2}here\b|\bdrop files?\b/i;
    var DROP_NAME = /drop[-_ ]?(zone|area|target)|file[-_ ]?drop|\bdropzone\b/i;
    var boxes = R.root.querySelectorAll('div, section, form, label, span, p');
    var drops = 0;
    for (var di = 0; di < boxes.length && di < 5000 && drops < 10; di++) {
      var d = boxes[di];
      var named = DROP_NAME.test(String(d.id || '') + ' ' + String(typeof d.className === 'string' ? d.className : ''));
      var says = !named && DROP_TEXT.test(String(d.innerText || '').slice(0, 300)) && String(d.innerText || '').length < 300;
      if (!named && !says) continue;
      // Only the innermost: a box whose child says the same is skipped.
      var inner = false;
      for (var dc = 0; dc < d.children.length && !inner; dc++) {
        var c = d.children[dc];
        inner = DROP_NAME.test(String(c.id || '') + ' ' + String(typeof c.className === 'string' ? c.className : '')) ||
          (says && DROP_TEXT.test(String(c.innerText || '').slice(0, 300)));
      }
      // A box with a file input in or near it, or a label for one, is
      // offered through that input.
      var around = d;
      if (around.parentElement) around = around.parentElement;
      var labelsFile = Array.prototype.some.call(d.querySelectorAll('label'), function(l) {
        return l.control && l.control.type === 'file';
      });
      if (inner || seen.has(d) || labelsFile || around.querySelector('input[type=file]')) continue;
      // A box a file could be dropped on, not a menu entry named after one.
      var db = d.getBoundingClientRect();
      if (db.width < 60 || db.height < 32 || d.closest('nav, header, footer, [role=navigation], [role=menu]')) continue;
      drops++;
      dropZone.add(d);
      consider(d, R.ox, R.oy);
    }
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
  // A label whose control is offered too would only split the vote: acting
  // on it acts on the control.
  var all = inDialog.concat(inView, offView);
  var offered = new Set(all);
  var chosen = all.filter(function(el) {
    return !replaced.has(el) && !(el.tagName === 'LABEL' && el.control && offered.has(el.control));
  }).slice(0, MAX);
  // Keep the offered elements in this isolated world, where page scripts
  // can't see them, under this extraction's token; actions look them up by
  // reference (see DEEP_FIND_JS). The last few extractions are kept, so a
  // late answer still lands on exactly the element it was offered.
  var store = globalThis.__bsPicks || (globalThis.__bsPicks = { order: [] });
  store[STAMP] = chosen;
  store.order.push(STAMP);
  while (store.order.length > 8) delete store[store.order.shift()];
  var elements = [];
  for (var j = 0; j < chosen.length; j++) {
    var id = 'e' + (j + 1);
    var desc = describe(chosen[j]);
    if (chosen[j].closest(DIALOG)) desc += ' (in dialog)';
    var rec = { id: id, desc: desc };
    var rg = chosen[j];
    if (rg.tagName === 'INPUT' && rg.type === 'radio' && rg.name) {
      rec.group = 'n:' + rg.name + ':' + (rg.form ? Array.prototype.indexOf.call(document.forms, rg.form) : -1);
    } else if (rg.getAttribute && rg.getAttribute('role') === 'radio' && rg.closest('[role=radiogroup]')) {
      var rgs = rg.closest('[role=radiogroup]');
      rgs.__bsGroup = rgs.__bsGroup || ('g:' + Math.random().toString(36).slice(2));
      rec.group = rgs.__bsGroup;
    }
    if (chosen[j].tagName === 'SELECT' && chosen[j].options) {
      var opts = [];
      for (var q = 0; q < chosen[j].options.length && q < 25; q++) {
        opts.push({ i: q, label: clamp(chosen[j].options[q].label, 60) });
      }
      rec.options = opts;
    }
    elements.push(rec);
  }
  // The page's own content, not its menus: navigation, sidebars, headers
  // and footers (a Wikipedia article's first 1,500 characters are nearly all
  // menu and table of contents) are left out, so a judgment like "is the
  // goal met?" reads what the page is about.
  var norm = function(s) { return String(s || '').replace(/\s+/g, ' ').trim(); };
  var textOf = function(root) {
    var chrome = new Set();
    root.querySelectorAll('nav, aside, header, footer, [role="navigation"], [role="complementary"], [role="banner"], [role="contentinfo"]').forEach(function(n) {
      String(n.innerText || '').split('\n').forEach(function(l) { l = norm(l); if (l) chrome.add(l); });
    });
    var t = String(root.innerText || '').split('\n').map(norm).filter(function(l) { return l && !chrome.has(l); }).join(' ');
    return t.length > 1500 ? t.slice(0, 1499) + '...' : t;
  };
  var modal = document.querySelector(DIALOG);
  var main = document.querySelector('main, [role="main"]');
  var text = '';
  if (modal && modal.getClientRects().length) text = textOf(modal);
  else if (main && main.getClientRects().length) text = textOf(main);
  else if (document.body) text = textOf(document.body);
  // The page's main heading leads, even where a site puts it inside a
  // <header> (Wikipedia's article title).
  var h1 = document.querySelector('h1');
  var heading = h1 && h1.getClientRects().length ? norm(h1.innerText) : '';
  if (heading && !(modal && modal.getClientRects().length) && text.indexOf(heading) !== 0) {
    text = ('Heading: ' + heading + '. ' + text).slice(0, 1500);
  }
  return {
    url: String(location.href),
    title: clamp(document.title, 120),
    text: text,
    elements: elements,
    omitted: Math.max(0, inDialog.length + inView.length + offView.length - chosen.length)
  };
}"#;

/// The extraction script, with [`MAX_ELEMENTS`] baked in.
static EXTRACT_FN: Lazy<String> = Lazy::new(|| {
    EXTRACT_FN_TEMPLATE
        .replace("__MAX_ELEMENTS__", &MAX_ELEMENTS.to_string())
        .replace("__ICON_HINT__", icon_hint_js!())
});

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

/// JS that finds an element, with the offset of its document from the top
/// viewport: a `jev-ref:` reference (see [`PageInfo::css`]) to an element an
/// extraction kept, or else the first match of a CSS selector in the page,
/// any open shadow root or any same-origin iframe. Defines `__bsDeep(sel)`
/// → `{ el, ox, oy }` or `null`; prepend it to a function body. Runs in the
/// isolated world, which holds the references.
pub const DEEP_FIND_JS: &str = r#"var __bsDeep = function(sel) {
  // In a frame scripts from the page around it can't reach, where this
  // world's viewport sits in the top one (see frames.rs).
  var base = globalThis.__bsBase || { x: 0, y: 0 };
  var ref = /^jev-ref:([0-9a-f]+):(\d+)$/.exec(sel);
  if (ref) {
    var picks = globalThis.__bsPicks && globalThis.__bsPicks[ref[1]];
    var el = picks && picks[+ref[2]];
    if (!el || !el.isConnected) return null;
    var ox = base.x, oy = base.y, w = el.ownerDocument.defaultView;
    while (w && w.frameElement) {
      var fe = w.frameElement, fr = fe.getBoundingClientRect();
      ox += fr.left + fe.clientLeft; oy += fr.top + fe.clientTop;
      w = w.parent;
    }
    return { el: el, ox: ox, oy: oy };
  }
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
  walk(document, base.x, base.y, 0);
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
    /// Reference to it for the action helpers, e.g. `jev-ref:1a2b3c4d:16`.
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
    /// Its reference when it isn't the default one [`PageInfo::css`] makes
    /// from the id: an element of a frame scripts can't reach, kept in that
    /// frame's world under a stamp of its own.
    pub css: Option<String>,
    /// The radio group it belongs to: any of a group's buttons stands for
    /// the group when picking from it.
    pub group: Option<String>,
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

    /// A reference to an element this extraction offered, for the action
    /// helpers' [`DEEP_FIND_JS`]: the element itself, kept in the isolated
    /// world rather than marked in the page. Finds nothing once the element
    /// left the page or eight newer extractions have run.
    pub fn css(&self, id: &str) -> String {
        if let Some(css) = self
            .elements
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.css.clone())
        {
            return css;
        }
        let n = id
            .strip_prefix('e')
            .and_then(|n| n.parse::<usize>().ok())
            .map_or(usize::MAX, |n| n - 1);
        format!("jev-ref:{}:{n}", self.stamp)
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
pub async fn resolve(page: &Page, goal: &str, by_group: bool) -> Result<Resolution> {
    let floor = config()?.min_confidence;
    resolve_with_floor(page, goal, floor, by_group).await
}

/// Resolve a goal on the automatic fallback path (a selector that matched
/// nothing as CSS). Applies the stricter `JEV_FALLBACK_MIN_CONFIDENCE` floor:
/// the caller asked for CSS, so a Jev guess should only act when it is
/// clearly right.
pub async fn resolve_fallback(page: &Page, goal: &str, by_group: bool) -> Result<Resolution> {
    let floor = config()?.fallback_min_confidence;
    resolve_with_floor(page, goal, floor, by_group).await
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
        match iso_eval_top(page, &script).await {
            Ok(raw) => match parse_extraction(&raw, &stamp) {
                Ok(mut info) => {
                    add_frames(page, &mut info, skip_typed).await;
                    return Ok(info);
                }
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

/// Add the elements of frames page scripts can't reach (an application
/// form embedded from another site) to `info`, after the page's own. Each
/// frame keeps its elements under a stamp of its own, so a reference finds
/// the element in its frame and nowhere else. A frame that fails to answer
/// is left out.
async fn add_frames(page: &Page, info: &mut PageInfo, skip_typed: bool) {
    for (k, frame) in crate::frames::closed(page).await.iter().enumerate() {
        let stamp = format!("{}{:02x}", info.stamp, k + 1);
        let Ok(raw) = crate::frames::eval(frame, &extract_fn(&stamp, skip_typed)).await else {
            continue;
        };
        let Ok(inner) = parse_extraction(&raw, &stamp) else {
            continue;
        };
        let host = frame.host();
        for e in &inner.elements {
            let css = inner.css(&e.id);
            info.elements.push(Element {
                id: format!("e{}", info.elements.len() + 1),
                desc: format!("{} (in the frame from {host})", e.desc),
                options: e.options.clone(),
                css: Some(css),
                group: e.group.as_ref().map(|g| format!("{stamp}:{g}")),
            });
        }
        info.omitted += inner.omitted;
        if !inner.text.is_empty() && info.text.len() < 3000 {
            info.text
                .push_str(&format!(" [frame from {host}] {}", inner.text));
        }
    }
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

/// With `by_group`, a radio button's confidence is its whole group's: the
/// caller picks within the group, so Jev splitting its vote among the
/// group's buttons is still one answer.
async fn resolve_with_floor(
    page: &Page,
    goal: &str,
    floor: f64,
    by_group: bool,
) -> Result<Resolution> {
    let cfg = config()?;
    let page_info = extract_with_retry(page, false).await?;
    if page_info.elements.is_empty() {
        bail!("no interactive elements found on the page — navigate or wait_for content first");
    }
    let checked = page_info.elements.len();
    let started = Instant::now();
    let pick = |info: &PageInfo| {
        let state = build_state(goal, info);
        let mut criteria = Map::new();
        for e in &info.elements {
            criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
        }
        criteria.insert(
            "__none__".to_string(),
            Value::String("None of these elements is what the agent wants.".to_string()),
        );
        let cfg = &cfg;
        let groups: std::collections::HashMap<String, String> = info
            .elements
            .iter()
            .filter_map(|e| Some((e.id.clone(), e.group.clone()?)))
            .collect();
        async move {
            let (id, mut confidence, tokens, model, ranked) =
                decide(cfg, &state, &criteria).await?;
            if by_group {
                if let Some(g) = groups.get(&id) {
                    confidence += ranked
                        .iter()
                        .filter(|(other, _)| other != &id && groups.get(other) == Some(g))
                        .map(|(_, p)| p)
                        .sum::<f64>();
                }
            }
            Ok::<_, anyhow::Error>((id, confidence.min(1.0), tokens, model))
        }
    };
    // What a person sees first (dialog and viewport first). Only a big page
    // whose first look found nothing good enough is narrowed in groups.
    let shown = checked.min(CHOICE_LIMIT - 1);
    let mut page_info = page_info;
    let mut first = page_info.subset(&(0..shown).collect::<Vec<_>>());
    first.omitted += checked - shown;
    let (mut element_id, mut confidence, mut input_tokens, mut model) = pick(&first).await?;
    if checked > shown && (element_id == "__none__" || confidence < floor) {
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
        page_info = page_info.subset(&keep);
        (element_id, confidence, input_tokens, model) = pick(&page_info).await?;
    } else {
        page_info = first;
    }
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
        "press_key",
        "Press a key on the keyboard: Enter to submit a search just typed, Escape to close a popup, Tab to move focus, an arrow, and so on.",
    ),
];

/// Extra actions a mobile session's `act` offers: swipes across the middle
/// of the screen, for drawers, carousels and pull-to-refresh that respond to
/// a finger anywhere.
pub const TOUCH_ACTIONS: &[(&str, &str)] = &[
    (
        "swipe_left",
        "Swipe a finger left across the carousel in view (else the middle of the screen), e.g. to move it on or close a drawer.",
    ),
    (
        "swipe_right",
        "Swipe a finger right across the carousel in view (else the middle of the screen), e.g. to move it back or open a side menu.",
    ),
    (
        "swipe_up",
        "Swipe a finger up the screen, e.g. to scroll down the way a phone does or dismiss a sheet.",
    ),
    (
        "swipe_down",
        "Swipe a finger down the screen, e.g. to scroll up, pull to refresh or close a bottom sheet.",
    ),
];

/// The actions an `act` step offers besides elements: the pseudo-actions,
/// plus swipes on a touch (mobile) session.
fn step_actions(touch: bool) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    PSEUDO_ACTIONS
        .iter()
        .chain(TOUCH_ACTIONS.iter().filter(move |_| touch))
}

/// Ways of acting on an element a touch session adds.
const TOUCH_OPERATIONS: &[(&str, &str)] = &[(
    "swipe",
    "Swipe across it with a finger, e.g. to move a carousel or close a drawer it belongs to.",
)];

/// Directions for a swipe, asked speculatively on touch sessions.
const DIRECTIONS: &[(&str, &str)] = &[
    ("left", "Left"),
    ("right", "Right"),
    ("up", "Up"),
    ("down", "Down"),
];

/// The keys `act`'s press_key action can press, asked speculatively in each
/// step's request so the answer is ready if the step picks press_key.
const KEYS: &[(&str, &str)] = &[
    ("Enter", "Enter: submit a search or form, confirm"),
    ("Escape", "Escape: close a popup, menu or dialog"),
    ("Tab", "Tab: move focus to the next control"),
    ("Shift+Tab", "Shift+Tab: move focus to the previous control"),
    ("ArrowDown", "Down arrow"),
    ("ArrowUp", "Up arrow"),
    ("ArrowLeft", "Left arrow"),
    ("ArrowRight", "Right arrow"),
    ("Space", "Space bar: toggle the focused checkbox or button"),
    (
        "Backspace",
        "Backspace: delete the character before the cursor",
    ),
    ("Delete", "Delete: delete the character after the cursor"),
    ("PageDown", "Page Down"),
    ("PageUp", "Page Up"),
    ("Home", "Home"),
    ("End", "End"),
];

/// The `state` string for an `act` step: the goal, the page's identity, what
/// was already done, and the numbered element menu. Recent actions keep the
/// loop from re-picking something that had no effect.
fn build_act_state(
    goal: &str,
    page: &PageInfo,
    recent: &[String],
    value_keys: &[String],
    touch: bool,
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
    lines.extend(step_actions(touch).map(|(id, desc)| format!("{id}: {desc}")));
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
fn act_criteria(page: &PageInfo, not_listed: bool, touch: bool) -> Map<String, Value> {
    let mut criteria = Map::new();
    for e in &page.elements {
        criteria.insert(e.id.clone(), Value::String(e.desc.clone()));
    }
    for (id, desc) in step_actions(touch) {
        criteria.insert((*id).to_string(), Value::String((*desc).to_string()));
    }
    if not_listed {
        criteria.insert(
            NOT_LISTED.to_string(),
            Value::String(
                "The right element is not listed here; it is among the further elements on the page."
                    .to_string(),
            ),
        );
    }
    criteria
}

/// The option a big page's first `act` request offers for "the right
/// element is among those not listed"; choosing it narrows the whole page.
const NOT_LISTED: &str = "not_listed";

/// One `act` step decision: what to do next, plus the side judgments that
/// all ride the same fan-out request.
pub struct StepDecision {
    /// The chosen id: an element's offered id ("e17"), or a pseudo-action name.
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
    /// The direction for a swipe on a touch session: left, right, up or down.
    pub direction: String,
    /// The key for a press_key step (one of [`KEYS`]), "Enter" unless Jev
    /// was sure of another.
    pub key: String,
    /// Speculative answers from the same request: for each offered text
    /// field (up to [`VALUE_FIELDS`]), the caller-provided value key that
    /// belongs in it, keyed by element id.
    pub field_values: std::collections::HashMap<String, String>,
    /// The step's general guess at which value is typed next, used when the
    /// chosen field's own answer is none.
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
                step_actions(true)
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
    stop_at: f64,
    touch: bool,
) -> Result<StepDecision> {
    let cfg = config()?;
    let mut page_info = extract_with_retry(page, true).await?;
    let reserved = step_actions(touch).count();
    let input = StepInput {
        goal,
        recent,
        value_keys,
        touch,
    };
    if page_info.elements.len() + reserved <= CHOICE_LIMIT {
        return step_on(&cfg, input, page_info, None, false).await;
    }
    // A big page: first offer what a person sees (dialog and viewport first)
    // with an option to say the right element isn't listed. Most steps end
    // here, in one request; only a "not listed" or unsure answer pays for
    // narrowing the whole page in groups.
    let shown = CHOICE_LIMIT - reserved - 1;
    let mut first = page_info.subset(&(0..shown).collect::<Vec<_>>());
    first.omitted += page_info.elements.len() - shown;
    // The page-level judgments also go out beside the page's text alone, in
    // parallel: next to a shortlist that still offers the way to the goal,
    // an achieved goal can read as still to do (see `page_gate_questions`).
    let head = act_state_head(goal, &page_info, recent, value_keys).join("\n");
    let gates_only = async {
        let body = json!({ "model": cfg.model, "state": head, "questions": page_gate_questions() });
        post_systemone(&cfg, &body).await
    };
    let (step, gates) = tokio::join!(step_on(&cfg, input, first, None, true), gates_only);
    let mut d = step?;
    let gates = gates?;
    d.goal_met = d.goal_met.max(parse_noul(&gates, "goal_met")?);
    d.login = parse_noul(&gates, "login")?;
    d.error = parse_noul(&gates, "error")?;
    d.bot_check = parse_noul(&gates, "bot_check")?;
    if d.goal_met >= stop_at || (d.chosen != NOT_LISTED && d.confidence >= ACT_MIN_CONFIDENCE) {
        return Ok(d);
    }
    let gates_json;
    {
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
        // The group request already judged the page: when it reads the goal
        // as met (at `stop_at`, the caller's stop threshold), the loop will
        // stop on it, so skip the step's own request.
        if let Some(json) = &first {
            if parse_noul(json, "goal_met")? >= stop_at {
                return Ok(StepDecision {
                    chosen: String::new(),
                    is_element: false,
                    description: String::new(),
                    confidence: 0.0,
                    goal_met: parse_noul(json, "goal_met")?,
                    stuck: 0.0,
                    login: parse_noul(json, "login")?,
                    error: parse_noul(json, "error")?,
                    irreversible: 0.0,
                    bot_check: parse_noul(json, "bot_check")?,
                    candidates: Vec::new(),
                    operation: "click".to_string(),
                    direction: "left".to_string(),
                    key: "Enter".to_string(),
                    field_values: std::collections::HashMap::new(),
                    value_key: None,
                    next_best: None,
                    has_elements: true,
                    page: page_info,
                });
            }
        }
        gates_json = first;
        // Every group said none: offer the page as it reads, dialog and
        // viewport first, and let the step scroll, wait or go back.
        if keep.is_empty() {
            keep = (0..CHOICE_LIMIT - reserved).collect();
        }
        page_info = page_info.subset(&keep);
    }
    step_on(&cfg, input, page_info, gates_json, false).await
}

/// The step request over `page_info`'s elements: the element Choice and
/// everything that rides along. `gates_json` carries page-level judgments
/// from a group request, when the page was narrowed; `not_listed` adds the
/// option to say the right element is among those left out.
/// What every request of one `act` step shares.
#[derive(Clone, Copy)]
struct StepInput<'a> {
    goal: &'a str,
    recent: &'a [String],
    value_keys: &'a [String],
    touch: bool,
}

async fn step_on(
    cfg: &Config,
    input: StepInput<'_>,
    page_info: PageInfo,
    gates_json: Option<Value>,
    not_listed: bool,
) -> Result<StepDecision> {
    let StepInput {
        goal,
        recent,
        value_keys,
        touch,
    } = input;
    let has_elements = !page_info.elements.is_empty();

    let state = build_act_state(goal, &page_info, recent, value_keys, touch);
    let criteria = act_criteria(&page_info, not_listed, touch);
    let mut questions = Map::new();
    // Always asked, even on a page with no elements: scrolling, waiting,
    // going back, keys and swipes need none.
    questions.insert(
            "element".to_string(),
            json!({
                "type": "choice",
                "instructions": "Which single listed element or action is the best next step toward the goal?",
                "criteria": criteria
            }),
        );
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
        for (id, desc) in OPERATIONS
            .iter()
            .chain(TOUCH_OPERATIONS.iter().filter(|_| touch))
        {
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
    if touch {
        let mut dirs = Map::new();
        for (id, desc) in DIRECTIONS {
            dirs.insert((*id).to_string(), Value::String((*desc).to_string()));
        }
        questions.insert(
            "direction".to_string(),
            json!({
                "type": "choice",
                "instructions": "If the best next step swipes a listed element, which way does the finger move?",
                "criteria": dirs
            }),
        );
    }
    let mut keys = Map::new();
    for (id, desc) in KEYS {
        keys.insert((*id).to_string(), Value::String((*desc).to_string()));
    }
    questions.insert(
        "key".to_string(),
        json!({
            "type": "choice",
            "instructions": "If the best next step is to press a key, which key does the goal call for?",
            "criteria": keys
        }),
    );
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
        questions.insert(
            "value_key".to_string(),
            json!({
                "type": "choice",
                "instructions": "If the next step is to type into a field, which provided value belongs there? Otherwise pick __none__.",
                "criteria": value_criteria
            }),
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
    let json = post_systemone(cfg, &body).await?;
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
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "element", &ids)?;
    let chosen = answer.chosen.clone();
    let confidence = answer.confidence;
    let is_element = chosen.starts_with('e') && page_info.elements.iter().any(|e| e.id == chosen);
    let description = if is_element {
        page_info
            .elements
            .iter()
            .find(|e| e.id == chosen)
            .map(|e| e.desc.clone())
            .unwrap_or_default()
    } else {
        step_actions(true)
            .find(|(id, _)| *id == chosen)
            .map(|(_, desc)| (*desc).to_string())
            .unwrap_or_else(|| chosen.clone())
    };
    let mut candidates = vec![(chosen.clone(), confidence)];
    candidates.extend(answer.ranked.iter().take(2).cloned());
    let next_best = answer
        .ranked
        .into_iter()
        .find(|(id, _)| id.starts_with('e') && page_info.elements.iter().any(|e| e.id == *id));
    let mut operation = "click".to_string();
    if has_elements {
        let ids: Vec<&str> = OPERATIONS
            .iter()
            .chain(TOUCH_OPERATIONS.iter().filter(|_| touch))
            .map(|(id, _)| *id)
            .collect();
        let answer = parse_choice(&json, "operation", &ids)?;
        if answer.confidence >= ACT_MIN_CONFIDENCE {
            operation = answer.chosen;
        }
    }
    let mut direction = "left".to_string();
    if touch {
        let ids: Vec<&str> = DIRECTIONS.iter().map(|(id, _)| *id).collect();
        let answer = parse_choice(&json, "direction", &ids)?;
        if answer.confidence >= ACT_MIN_CONFIDENCE {
            direction = answer.chosen;
        }
    }
    let key_ids: Vec<&str> = KEYS.iter().map(|(id, _)| *id).collect();
    let key_answer = parse_choice(&json, "key", &key_ids)?;
    let key = if key_answer.confidence >= ACT_MIN_CONFIDENCE {
        key_answer.chosen
    } else {
        "Enter".to_string()
    };
    let mut field_values = std::collections::HashMap::new();
    let ids: Vec<&str> = value_criteria.keys().map(|k| k.as_str()).collect();
    let mut value_key = None;
    if !value_fields.is_empty() {
        let answer = parse_choice(&json, "value_key", &ids)?;
        if answer.chosen != "__none__" && answer.confidence >= ACT_MIN_CONFIDENCE {
            value_key = Some(answer.chosen);
        }
    }
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
        direction,
        key,
        field_values,
        value_key,
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

/// How much visible text `ask` sends: enough for a long article's opening
/// and most app screens, well under Jev's 32k-token question context.
const ASK_TEXT_CHARS: usize = 20_000;

/// The page as `ask` reads it: URL, title and visible text, an open
/// dialog's text first.
const ASK_READ_FN: &str = r#"function() {
  var MAX = __MAX__;
  var textOf = function(root) { return String(root.innerText || '').replace(/\s+/g, ' ').trim(); };
  var modal = document.querySelector('dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]');
  var text = '';
  if (modal && modal.getClientRects().length) text = 'Open dialog: ' + textOf(modal) + '\n\n';
  if (document.body) text += textOf(document.body);
  if (text.length > MAX) text = text.slice(0, MAX - 3) + '...';
  return { url: String(location.href), title: String(document.title), text: text };
}"#;

/// `ask`'s answer about the page.
pub struct AskAnswer {
    /// "yes" or "no" for a yes/no question, else the chosen option.
    pub answer: String,
    /// For a yes/no question, the probability of yes; for options, Jev's
    /// confidence in the chosen one.
    pub probability: f64,
    /// Every option's probability, most likely first (options only).
    pub probabilities: Vec<(String, f64)>,
}

/// Judge the page as it is now: a yes/no question (a Noul) when `options` is
/// empty, else which option fits (a Choice). One request; any Jev problem is
/// an error, never a default answer.
pub async fn ask(page: &Page, question: &str, options: &[String]) -> Result<AskAnswer> {
    let cfg = config()?;
    let read = ASK_READ_FN.replace("__MAX__", &ASK_TEXT_CHARS.to_string());
    let v = iso_eval_function(page, &read)
        .await
        .context("reading the page")?;
    let url = v.get("url").and_then(|v| v.as_str()).unwrap_or_default();
    let title = v.get("title").and_then(|v| v.as_str()).unwrap_or_default();
    let text = v.get("text").and_then(|v| v.as_str()).unwrap_or_default();
    let state = format!("Page: {url} — {title}\nVisible text: {text}");
    if options.is_empty() {
        let body = json!({
            "model": cfg.model,
            "state": state,
            "questions": { "answer": { "type": "noul", "instructions": question } }
        });
        let json = post_systemone(&cfg, &body).await?;
        let p = parse_noul(&json, "answer")?;
        return Ok(AskAnswer {
            answer: if p >= 0.5 { "yes" } else { "no" }.to_string(),
            probability: p,
            probabilities: Vec::new(),
        });
    }
    let mut criteria = Map::new();
    for (i, o) in options.iter().enumerate() {
        criteria.insert(format!("o{i}"), Value::String(o.clone()));
    }
    let body = json!({
        "model": cfg.model,
        "state": state,
        "questions": {
            "answer": { "type": "choice", "instructions": question, "criteria": criteria }
        }
    });
    let json = post_systemone(&cfg, &body).await?;
    let ids: Vec<&str> = criteria.keys().map(|k| k.as_str()).collect();
    let answer = parse_choice(&json, "answer", &ids)?;
    let label = |id: &str| {
        id.strip_prefix('o')
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| options.get(n))
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };
    let mut probabilities = Vec::new();
    if !answer.ranked.is_empty() {
        let chosen_p = 1.0 - answer.ranked.iter().map(|(_, p)| p).sum::<f64>();
        probabilities.push((label(&answer.chosen), chosen_p.max(0.0)));
        probabilities.extend(answer.ranked.iter().map(|(id, p)| (label(id), *p)));
    }
    Ok(AskAnswer {
        answer: label(&answer.chosen),
        probability: answer.confidence,
        probabilities,
    })
}

/// `read` returns a page under this many characters whole, without asking
/// Jev: pruning it wouldn't be worth a request.
const READ_WHOLE_BELOW: usize = 8_000;

/// Passage characters per `read` request, keeping each well inside Jev's
/// 64k-token context. Longer pages send several requests in parallel.
const READ_CHARS_PER_REQUEST: usize = 50_000;

/// The page as `read` splits it: visible text in passages of about 600
/// characters, cut at the text's own line breaks, each under the heading it
/// follows, leaving out navigation, sidebars, headers and footers. An open
/// dialog is read instead of the page.
const READ_SPLIT_FN: &str = r#"function() {
  var DIALOG = 'dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]';
  var modal = document.querySelector(DIALOG);
  var root = modal && modal.getClientRects().length ? modal : document.body;
  if (!root) return { url: String(location.href), title: String(document.title), chars: 0, passages: [] };
  var norm = function(s) { return String(s || '').replace(/\s+/g, ' ').trim(); };
  var headings = new Set();
  root.querySelectorAll('h1, h2, h3, h4, h5, h6').forEach(function(h) { var t = norm(h.innerText); if (t) headings.add(t); });
  // Navigation, sidebars, headers and footers (a table of contents, menus)
  // repeat the headings and say nothing about the content.
  var chrome = new Set();
  root.querySelectorAll('nav, aside, header, footer, [role="navigation"], [role="complementary"], [role="banner"], [role="contentinfo"]').forEach(function(n) {
    String(n.innerText || '').split('\n').forEach(function(l) { l = norm(l); if (l) chrome.add(l); });
  });
  var lines = String(root.innerText || '').split('\n').map(norm).filter(function(l) { return l && !chrome.has(l); });
  var passages = [], heading = '', buf = '', chars = 0;
  var flush = function() { if (buf) passages.push({ h: heading, t: buf }); buf = ''; };
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    chars += line.length + 1;
    if (headings.has(line)) { flush(); heading = line; continue; }
    if (buf && buf.length + line.length > 600) flush();
    buf = buf ? buf + '\n' + line : line;
  }
  flush();
  return { url: String(location.href), title: String(document.title), chars: chars, passages: passages };
}"#;

/// One passage `read` kept.
pub struct ReadPassage {
    /// The heading it falls under, if any.
    pub heading: String,
    pub text: String,
    /// Jev's probability that it helps answer the question; 1 when the page
    /// was short enough to return whole.
    pub relevance: f64,
}

/// What `read` found.
pub struct ReadResult {
    pub url: String,
    pub title: String,
    /// Visible characters on the page.
    pub page_chars: usize,
    /// Passages the page was split into.
    pub total: usize,
    /// The passages returned, in page order.
    pub passages: Vec<ReadPassage>,
    /// Whether Jev filtered the page (false: short enough to return whole).
    pub filtered: bool,
}

/// Return only the parts of the page that help answer `question`: one Noul
/// per passage, in parallel requests, keeping those Jev rates at even odds
/// or better, most relevant first if they don't all fit in `max_chars`,
/// then back in page order. A short page comes back whole.
pub async fn read(page: &Page, question: &str, max_chars: usize) -> Result<ReadResult> {
    let v = iso_eval_function(page, READ_SPLIT_FN)
        .await
        .context("reading the page")?;
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
    let page_chars = v.get("chars").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let all: Vec<(String, String)> = v
        .get("passages")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .map(|p| {
                    let s = |k: &str| {
                        p.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    (s("h"), s("t"))
                })
                .collect()
        })
        .unwrap_or_default();
    let total = all.len();
    if page_chars < READ_WHOLE_BELOW.min(max_chars) {
        let passages = all
            .into_iter()
            .map(|(heading, text)| ReadPassage {
                heading,
                text,
                relevance: 1.0,
            })
            .collect();
        return Ok(ReadResult {
            url,
            title,
            page_chars,
            total,
            passages,
            filtered: false,
        });
    }
    let cfg = config()?;
    let state = format!("Page: {url} — {title}");
    // Batch passages into requests by size.
    let mut batches: Vec<Vec<usize>> = vec![Vec::new()];
    let mut size = 0;
    for (i, (h, t)) in all.iter().enumerate() {
        if size + h.len() + t.len() > READ_CHARS_PER_REQUEST
            && !batches.last().is_some_and(Vec::is_empty)
        {
            batches.push(Vec::new());
            size = 0;
        }
        size += h.len() + t.len();
        batches.last_mut().expect("one batch").push(i);
    }
    let requests = batches.iter().map(|batch| {
        let mut questions = Map::new();
        for &i in batch {
            let (h, t) = &all[i];
            let under = if h.is_empty() {
                String::new()
            } else {
                format!(" (under the heading \"{h}\")")
            };
            questions.insert(
                format!("p{i}"),
                json!({
                    "type": "noul",
                    "instructions": format!(
                        "This passage{under} helps answer the question \"{question}\". Passage: {t}"
                    )
                }),
            );
        }
        let body = json!({ "model": cfg.model, "state": state, "questions": questions });
        let cfg = &cfg;
        async move {
            let json = post_systemone(cfg, &body).await?;
            batch
                .iter()
                .map(|&i| Ok((i, parse_noul(&json, &format!("p{i}"))?)))
                .collect::<Result<Vec<(usize, f64)>>>()
        }
    });
    let mut scored: Vec<(usize, f64)> = futures::future::try_join_all(requests)
        .await?
        .into_iter()
        .flatten()
        .filter(|(_, p)| *p >= 0.5)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut budget = max_chars;
    let mut kept: Vec<(usize, f64)> = Vec::new();
    for (i, p) in scored {
        let len = all[i].0.len() + all[i].1.len();
        if len > budget {
            continue;
        }
        budget -= len;
        kept.push((i, p));
    }
    kept.sort_by_key(|(i, _)| *i);
    let passages = kept
        .into_iter()
        .map(|(i, relevance)| ReadPassage {
            heading: all[i].0.clone(),
            text: all[i].1.clone(),
            relevance,
        })
        .collect();
    Ok(ReadResult {
        url,
        title,
        page_chars,
        total,
        passages,
        filtered: true,
    })
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
) -> Result<(String, f64, u64, String, Vec<(String, f64)>)> {
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
    Ok((
        answer.chosen,
        answer.confidence,
        input_tokens,
        model,
        answer.ranked,
    ))
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
                    Some(Element {
                        id,
                        desc,
                        options,
                        css: None,
                        group: e.get("group").and_then(|g| g.as_str()).map(str::to_string),
                    })
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
                    css: None,
                    group: None,
                },
                Element {
                    id: "e2".to_string(),
                    desc: "button \"Log in\"".to_string(),
                    options: vec![],
                    css: None,
                    group: None,
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
                css: None,
                group: None,
            }],
            omitted: 0,
            stamp: "ab12".to_string(),
        };
        let state = build_act_state(
            "search for oolong tea",
            &page,
            &["typed into the search box".to_string()],
            &["query".to_string()],
            false,
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
        let state = build_act_state("confirm we are on the empty page", &page, &[], &[], false);
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
                    css: None,
                    group: None,
                })
                .collect(),
            omitted: 0,
            stamp: "ab12".to_string(),
        };
        let criteria = act_criteria(&page, false, false);
        assert_eq!(criteria.len(), CHOICE_LIMIT);
        let page = page.subset(&(0..CHOICE_LIMIT - PSEUDO_ACTIONS.len() - 1).collect::<Vec<_>>());
        assert_eq!(act_criteria(&page, true, false).len(), CHOICE_LIMIT);
        let touch_page =
            page.subset(&(0..CHOICE_LIMIT - step_actions(true).count() - 1).collect::<Vec<_>>());
        assert_eq!(act_criteria(&touch_page, true, true).len(), CHOICE_LIMIT);
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
        assert_eq!(page.css("e2"), "jev-ref:ab12:1");
    }
}
