//! User-Agent + Sec-CH-UA-* override so `HeadlessChrome` never leaks.
//!
//! The metadata is kept as a raw JSON Value matching CDP's
//! `Network.UserAgentMetadata` shape — that way we can serialize/deserialize
//! it through the state file and the chromiumoxide-typed
//! `SetUserAgentOverrideParams` without maintaining parallel struct
//! definitions.
use anyhow::{Context, Result, anyhow};
use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::browser::GetVersionParams;
use once_cell::sync::OnceCell;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct UaOverride {
    pub user_agent: String,
    /// CDP `Network.UserAgentMetadata` as JSON.
    pub metadata: Value,
}

static CHROME_VERSION: OnceCell<ChromeVersion> = OnceCell::new();

#[derive(Debug, Clone)]
struct ChromeVersion {
    full: String,
    major: String,
}

async fn chrome_version(browser: &Browser) -> Result<ChromeVersion> {
    if let Some(v) = CHROME_VERSION.get() {
        return Ok(v.clone());
    }
    let result = browser
        .execute(GetVersionParams::default())
        .await
        .context("Browser.getVersion")?;
    // `product` looks like "HeadlessChrome/146.0.7680.31"; the version is the
    // tail after the slash.
    let product = &result.result.product;
    let full = product
        .split_once('/')
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| "146.0.7680.31".to_string());
    let major = full
        .split('.')
        .next()
        .ok_or_else(|| anyhow!("unexpected version string {full}"))?
        .to_string();
    let v = ChromeVersion { full, major };
    let _ = CHROME_VERSION.set(v.clone());
    Ok(v)
}

pub async fn resolve(browser: &Browser, use_mobile: bool) -> Result<UaOverride> {
    let ChromeVersion { full, major } = chrome_version(browser).await?;
    // GREASE brand must match what this Chrome actually emits, or client-hint
    // consistency checks flag it. Chrome 146 uses `Not-A.Brand;v="24"` (seen in
    // the real headless UA-CH). Update if a future Chrome rotates it.
    let brands = json!([
        { "brand": "Chromium", "version": major },
        { "brand": "Google Chrome", "version": major },
        { "brand": "Not-A.Brand", "version": "24" },
    ]);
    let full_version_list = json!([
        { "brand": "Chromium", "version": full },
        { "brand": "Google Chrome", "version": full },
        { "brand": "Not-A.Brand", "version": "24.0.0.0" },
    ]);

    if use_mobile {
        Ok(UaOverride {
            user_agent: format!(
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Mobile Safari/537.36"
            ),
            metadata: json!({
                "brands": brands,
                "fullVersionList": full_version_list,
                "platform": "Android",
                "platformVersion": "14",
                "architecture": "",
                "model": "Pixel 8",
                "mobile": true,
                "formFactors": ["Mobile"],
            }),
        })
    } else {
        Ok(UaOverride {
            user_agent: format!(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
            ),
            metadata: json!({
                "brands": brands,
                "fullVersionList": full_version_list,
                "platform": "Linux",
                "platformVersion": "",
                "architecture": "x86",
                "model": "",
                "mobile": false,
                "formFactors": ["Desktop"],
            }),
        })
    }
}

/// Init script run before any page script, on every target (the listener applies
/// it to all tabs; the MCP also applies it to its own). Masks the JS-visible
/// headless tells with a Linux/desktop profile coherent with the box's real
/// platform: no `navigator.webdriver`, a real Chrome PDF-viewer plugin set,
/// and `window.chrome.runtime`. WebGL is NOT spoofed —
/// Chrome renders on the real NVIDIA GPU (chrome.nix), so the genuine renderer
/// passes consistency checks a software-GL spoof fails. Every patch is
/// try/caught so a double-apply or failure can't break the page.
pub const STEALTH_JS: &str = r#"(() => {
  try { Object.defineProperty(navigator, 'webdriver', { get: () => false }); } catch (e) {}
  // NOTE: navigator.languages is deliberately NOT overridden. This script runs
  // only in the main world; Web Workers get a fresh context with the browser's
  // real navigator.languages (["en-US"]). Faking ["en-US","en"] here made the
  // main thread disagree with workers — deviceandbrowserinfo's
  // hasInconsistentWorkerValues. Reading the real value keeps them consistent.
  // (To present ["en-US","en"] everywhere instead, set Chrome --accept-lang at
  // launch in chrome.nix so it applies browser-wide, main AND workers.)
  try { Object.defineProperty(navigator, 'pdfViewerEnabled', { get: () => true }); } catch (e) {}
  try {
    const mk = (name) => {
      const p = Object.create(Plugin.prototype);
      Object.defineProperties(p, {
        name: { value: name, enumerable: true },
        description: { value: 'Portable Document Format', enumerable: true },
        filename: { value: 'internal-pdf-viewer', enumerable: true },
        length: { value: 1 },
      });
      return p;
    };
    const arr = ['PDF Viewer', 'Chrome PDF Viewer', 'Chromium PDF Viewer', 'Microsoft Edge PDF Viewer', 'WebKit built-in PDF'].map(mk);
    Object.defineProperty(navigator, 'plugins', { get: () => arr });
  } catch (e) {}
  try { window.chrome = window.chrome || {}; window.chrome.runtime = window.chrome.runtime || {}; } catch (e) {}
  try {
    const q = navigator.permissions && navigator.permissions.query;
    if (q) navigator.permissions.query = (p) =>
      p && p.name === 'notifications'
        ? Promise.resolve({ state: Notification.permission })
        : q(p);
  } catch (e) {}
  // NOTE: no WebGL renderer spoof — Chrome renders on the real NVIDIA GPU
  // (see chrome.nix), so the genuine renderer string passes the consistency
  // checks that a software-GL spoof fails (iphey flagged the spoof).
})();"#;

/// Loopback URL patterns blocked per-target via `Network.setBlockedURLs` — so an
/// in-page port scan can't probe the DevTools port (9222), the takeover daemon
/// (9223), or any automation/VNC port: every loopback request fails uniformly
/// with ERR_BLOCKED_BY_CLIENT, leaking no open/closed signal. No host firewall.
pub const BLOCKED_LOOPBACK: &[&str] = &[
    "*://localhost/*",
    "*://localhost:*",
    "*://127.0.0.1/*",
    "*://127.0.0.1:*",
    "*://[::1]/*",
    "*://[::1]:*",
    "*://0.0.0.0/*",
    "*://0.0.0.0:*",
];
