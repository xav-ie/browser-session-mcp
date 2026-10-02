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
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct UaOverride {
    pub user_agent: String,
    /// CDP `Network.UserAgentMetadata` as JSON.
    pub metadata: Value,
}

#[derive(Debug, Clone)]
struct ChromeVersion {
    full: String,
    major: String,
}

/// Asked per call rather than cached for the process: the listener outlives
/// Chrome upgrades, and a UA whose version doesn't match the engine is itself
/// a bot signal. It's one cheap CDP round trip per session open or reconnect.
async fn chrome_version(browser: &Browser) -> Result<ChromeVersion> {
    let result = browser
        .execute(GetVersionParams::default())
        .await
        .context("Browser.getVersion")?;
    // `product` looks like "HeadlessChrome/154.0.8037.92"; the version is the
    // tail after the slash.
    let product = &result.result.product;
    let full = product
        .split_once('/')
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| anyhow!("unexpected Chrome product string {product}"))?;
    let major = full
        .split('.')
        .next()
        .ok_or_else(|| anyhow!("unexpected version string {full}"))?
        .to_string();
    Ok(ChromeVersion { full, major })
}

/// Chrome's `Sec-CH-UA` brand list for a major version, as the engine itself
/// builds it (Chromium's `GenerateBrandVersionList`): the GREASE brand's
/// punctuation, version and position all rotate with the major version, so a
/// hardcoded list goes stale (and looks forged) on the next upgrade. With
/// `full`, versions are the full form (`fullVersionList`).
fn brand_list(major: &str, full: Option<&str>) -> Value {
    const CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let seed: usize = major.parse().unwrap_or(0);
    let grease_brand = format!("Not{}A{}Brand", CHARS[seed % 11], CHARS[(seed + 1) % 11]);
    let grease_version = match full {
        Some(_) => format!("{}.0.0.0", VERSIONS[seed % 3]),
        None => VERSIONS[seed % 3].to_string(),
    };
    let v = full.unwrap_or(major);
    let order = ORDERS[seed % 6];
    let mut list = vec![Value::Null; 3];
    list[order[0]] = json!({ "brand": grease_brand, "version": grease_version });
    list[order[1]] = json!({ "brand": "Chromium", "version": v });
    list[order[2]] = json!({ "brand": "Google Chrome", "version": v });
    Value::Array(list)
}

pub async fn resolve(browser: &Browser, use_mobile: bool) -> Result<UaOverride> {
    let ChromeVersion { full, major } = chrome_version(browser).await?;
    // Must match what this Chrome actually emits, or client-hint consistency
    // checks flag it; derived from the version rather than hardcoded.
    let brands = brand_list(&major, None);
    let full_version_list = brand_list(&major, Some(&full));

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

#[cfg(test)]
mod tests {
    use super::brand_list;

    fn brands(major: &str) -> Vec<(String, String)> {
        brand_list(major, None)
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                (
                    b["brand"].as_str().unwrap().into(),
                    b["version"].as_str().unwrap().into(),
                )
            })
            .collect()
    }

    // Expected lists are real Sec-CH-UA headers captured from
    // chrome-headless-shell (which says "HeadlessChrome" where branded Chrome
    // says "Google Chrome").
    #[test]
    fn grease_matches_real_chrome() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        // 146: "Chromium";v="146", "Not-A.Brand";v="24", "HeadlessChrome";v="146"
        assert_eq!(
            brands("146"),
            [
                s("Chromium", "146"),
                s("Not-A.Brand", "24"),
                s("Google Chrome", "146")
            ]
        );
        // 154: "Chromium";v="154", "HeadlessChrome";v="154", "Not A(Brand";v="99"
        assert_eq!(
            brands("154"),
            [
                s("Chromium", "154"),
                s("Google Chrome", "154"),
                s("Not A(Brand", "99")
            ]
        );
        let full = brand_list("154", Some("154.0.8037.92"));
        assert_eq!(full[2]["version"], "99.0.0.0");
        assert_eq!(full[0]["version"], "154.0.8037.92");
    }
}
