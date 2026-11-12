/// `__bsIcon(el)`: what an icon-only control shows, read from the markup
/// around the icon when the page gives it no name: an SVG's `<title>`, an
/// image's alt text or file name, an SVG sprite's `<use href>`, attributes
/// such as `data-icon` or `data-testid`, and icon library classes
/// (`fa-trash`, `lucide-x`, `bi-pencil`). Returns '' when nothing names it.
/// A hint, not a name: descriptions show it as `(icon "trash")`. A macro so
/// the scripts that describe elements (Jev's extraction, action results,
/// snapshots) paste the same text in at compile time.
macro_rules! icon_hint_js {
    () => {
        r#"var __bsIcon = function(el) {
  if (!el || !el.querySelector) return '';
  var NOISE = /^(icons?|ico|svg|img|image|fa[srlbdtk]?|fa(solid|regular|light|thin|duotone|brands)|solid|regular|outline|outlined|filled|fill|line|rounded|sharp|mini|micro|bi|lucide|mdi|ti|tabler|ri|bxs?|ph|feather|octicon|heroicons?|glyphicon|material|symbols|button|btn|action|sm|md|lg|xl|xs|\d+(px)?|w\d+|h\d+)$/;
  var clean = function(s) {
    s = String(s || '').replace(/^.*[#\/]/, '').replace(/\.(svg|png|gif|webp|jpe?g|ico)(\?.*)?$/i, '')
      .replace(/([a-z])([A-Z])/g, '$1 $2').toLowerCase();
    var words = s.split(/[\s_\-.:]+/).filter(function(w) { return w && !NOISE.test(w); });
    s = words.join(' ').trim();
    return s.length <= 30 ? s : '';
  };
  var t = el.querySelector('svg title') || (el.tagName === 'svg' ? el.querySelector('title') : null);
  if (t && t.textContent.trim()) return t.textContent.trim().slice(0, 40);
  var img = el.tagName === 'IMG' ? el : el.querySelector('img');
  if (img) {
    if (String(img.alt || '').trim()) return img.alt.trim().slice(0, 40);
    var fromSrc = clean(img.getAttribute('src'));
    if (fromSrc) return fromSrc;
  }
  var use = el.querySelector('use');
  if (use) {
    var fromUse = clean(use.getAttribute('href') || use.getAttribute('xlink:href'));
    if (fromUse) return fromUse;
  }
  var nodes = [el].concat(Array.prototype.slice.call(el.querySelectorAll('*'), 0, 12));
  var attrs = ['data-icon', 'data-testid', 'data-test-id', 'data-test', 'data-qa', 'data-action', 'data-cy'];
  for (var i = 0; i < nodes.length; i++) {
    for (var a = 0; a < attrs.length; a++) {
      var fromAttr = clean(nodes[i].getAttribute(attrs[a]));
      if (fromAttr) return fromAttr;
    }
  }
  var LIB = /^(fa|bi|lucide|mdi|ti|ri|bxs?|ph|icon|icons|glyphicon|octicon|feather|heroicon|svg-icon|ico)[-_]|[-_]icon$/i;
  for (var j = 0; j < nodes.length; j++) {
    var cls = String(nodes[j].getAttribute('class') || '').split(/\s+/);
    for (var k = 0; k < cls.length; k++) {
      if (!LIB.test(cls[k])) continue;
      var fromClass = clean(cls[k]);
      if (fromClass) return fromClass;
    }
  }
  return '';
};"#
    };
}

pub mod attach_guard;
pub mod child_capture;
pub mod chrome;
pub mod chrome_ctx;
pub mod devices;
pub mod emulation;
pub mod frames;
pub mod jev;
pub mod listener;
pub mod logs;
pub mod mcp;
pub mod measure;
pub mod profiler;
pub mod reaper;
pub mod redact;
pub mod saved_states;
pub mod server;
pub mod sessions;
pub mod snapshot;
pub mod state;
pub mod takeover;
pub mod takeover_relay;
pub mod user_agent;

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Path env vars that `STATE_DIR` replaced. Refusing to start while one is set
/// turns a half-migrated config into a startup error instead of processes
/// silently reading and writing different directories.
const REPLACED_VARS: [&str; 4] = ["STATE_FILE", "LOGS_DIR", "STATES_DIR", "TAKEOVER_DIR"];

/// Resolve the required `STATE_DIR`, which every role derives its paths from
/// (`sessions/`, `logs/`, `states/`, `takeover/`). One variable means the
/// cooperating processes can't disagree about where shared files live.
pub fn init_state_dir() -> Result<()> {
    if let Some(old) = REPLACED_VARS.iter().find(|v| std::env::var_os(v).is_some()) {
        bail!(
            "{old} is no longer supported; set STATE_DIR (all paths derive from it) and unset {old}"
        );
    }
    let dir = std::env::var_os("STATE_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("STATE_DIR is required (e.g. /var/lib/browser-session-mcp)")
        })?;
    let _ = STATE_DIR.set(PathBuf::from(dir));
    Ok(())
}

/// The directory from [`init_state_dir`], which `main` calls before any role runs.
pub fn state_dir() -> &'static Path {
    STATE_DIR
        .get()
        .expect("init_state_dir must run before state_dir")
}

/// The folder agents save files into for `upload_file`'s `path`
/// (`STATE_DIR/uploads`), when it exists. The NixOS module creates it
/// writable by every local user and clears files older than a day, so a
/// file reaches the page without its bytes passing through the model.
pub fn uploads_dir() -> Option<PathBuf> {
    STATE_DIR
        .get()
        .map(|d| d.join("uploads"))
        .filter(|p| p.is_dir())
}

/// Install the tracing subscriber for a subcommand. Always logs to stderr:
/// the `mcp` subcommand speaks the MCP protocol over stdout, and stderr is
/// captured by journald for the daemons all the same.
pub fn init_tracing(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .init();
}
