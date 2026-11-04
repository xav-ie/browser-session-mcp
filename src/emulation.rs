//! Per-session CPU/network throttling and init scripts.
//!
//! The MCP stores them in the session record; the attach guard applies them
//! to every tab of the session, on attach (while the tab is paused, so before
//! its first request and first script) and again whenever the record changes.
//! The guard holds each tab's CDP session for the tab's lifetime, which these
//! overrides need: Chrome drops them when the session that set them detaches.
//! Once a change is in place on every tab, the guard writes the record's
//! revision to `applied_path`, which the MCP waits for.
use anyhow::{Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::state::sessions_dir;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Emulation {
    /// CPU slowdown factor; 1 means no throttling.
    #[serde(default = "one")]
    pub cpu_throttling_rate: f64,
    /// Network conditions; absent means no throttling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConditions>,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConditions {
    /// Preset this came from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub offline: bool,
    /// Added round-trip latency in milliseconds.
    pub latency_ms: f64,
    /// Download bandwidth in kilobits per second.
    pub download_kbps: f64,
    /// Upload bandwidth in kilobits per second.
    pub upload_kbps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InitScript {
    pub id: String,
    pub source: String,
}

/// Chrome DevTools' throttling presets (front_end/core/sdk/NetworkManager.ts),
/// including its latency and bandwidth adjustment factors.
pub const PRESETS: &[(&str, f64, f64, f64)] = &[
    ("slow3g", 2000.0, 400.0, 400.0),
    ("slow4g", 562.5, 1440.0, 675.0),
    ("fast4g", 165.0, 8100.0, 1350.0),
];

pub fn preset(name: &str) -> Option<NetworkConditions> {
    if name == "offline" {
        return Some(NetworkConditions {
            preset: Some("offline".into()),
            offline: true,
            latency_ms: 0.0,
            download_kbps: 0.0,
            upload_kbps: 0.0,
        });
    }
    PRESETS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(
            |&(n, latency_ms, download_kbps, upload_kbps)| NetworkConditions {
                preset: Some(n.into()),
                offline: false,
                latency_ms,
                download_kbps,
                upload_kbps,
            },
        )
}

/// The `network` tool argument: a preset name, `"off"`, or custom values.
/// `Ok(None)` clears throttling.
pub fn parse_network(v: &Value) -> Result<Option<NetworkConditions>> {
    if let Some(name) = v.as_str() {
        if name == "off" {
            return Ok(None);
        }
        return preset(name).map(Some).ok_or_else(|| {
            anyhow!("unknown network preset {name:?}; use off, offline, slow3g, slow4g, fast4g or an object")
        });
    }
    let obj = v.as_object().ok_or_else(|| {
        anyhow!("network must be a preset name or {{latencyMs, downloadKbps, uploadKbps}}")
    })?;
    let num = |k: &str| -> Result<f64> {
        let n = obj
            .get(k)
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("network.{k} must be a number"))?;
        if !n.is_finite() || n < 0.0 {
            bail!("network.{k} must be zero or more");
        }
        Ok(n)
    };
    let net = NetworkConditions {
        preset: None,
        offline: false,
        latency_ms: num("latencyMs")?,
        download_kbps: num("downloadKbps")?,
        upload_kbps: num("uploadKbps")?,
    };
    if net.download_kbps == 0.0 || net.upload_kbps == 0.0 {
        bail!("network bandwidth must be above zero; use \"offline\" to cut the network");
    }
    Ok(Some(net))
}

/// What the guard has put in place on one tab.
#[derive(Debug, Clone, Default)]
pub struct TabApplied {
    pub cpu: Option<f64>,
    pub network: Option<NetworkConditions>,
    /// Init script id -> Chrome's identifier, `None` while the add is in flight.
    pub scripts: BTreeMap<String, Option<String>>,
    pub page_enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Send(&'static str, Value),
    /// `Page.addScriptToEvaluateOnNewDocument` for this init script id.
    AddScript(String, Value),
}

/// The commands that move a tab from `applied` to the session's `emulation`
/// and `scripts`, updating `applied` to match. Unchanged settings send
/// nothing, so a tab that is already current gets no commands.
pub fn plan(emulation: &Emulation, scripts: &[InitScript], applied: &mut TabApplied) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    // A tab the guard never touched runs unthrottled.
    let rate = emulation.cpu_throttling_rate.max(1.0);
    if applied.cpu.unwrap_or(1.0) != rate {
        cmds.push(Cmd::Send(
            "Emulation.setCPUThrottlingRate",
            json!({ "rate": rate }),
        ));
    }
    applied.cpu = Some(rate);

    if applied.network != emulation.network {
        cmds.push(Cmd::Send(
            "Network.emulateNetworkConditions",
            network_params(emulation.network.as_ref()),
        ));
    }
    applied.network = emulation.network.clone();

    let wanted: BTreeMap<&str, &InitScript> = scripts.iter().map(|s| (s.id.as_str(), s)).collect();
    let stale: Vec<String> = applied
        .scripts
        .keys()
        .filter(|id| !wanted.contains_key(id.as_str()))
        .cloned()
        .collect();
    for id in stale {
        if let Some(Some(identifier)) = applied.scripts.remove(&id) {
            cmds.push(Cmd::Send(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({ "identifier": identifier }),
            ));
        }
    }
    for (id, script) in wanted {
        if applied.scripts.contains_key(id) {
            continue;
        }
        if !applied.page_enabled {
            // Without Page enabled on this CDP session Chrome accepts the
            // script but never runs it.
            cmds.push(Cmd::Send("Page.enable", json!({})));
            applied.page_enabled = true;
        }
        cmds.push(Cmd::AddScript(
            id.to_string(),
            json!({ "source": script.source }),
        ));
        applied.scripts.insert(id.to_string(), None);
    }
    cmds
}

fn network_params(net: Option<&NetworkConditions>) -> Value {
    match net {
        Some(n) => json!({
            "offline": n.offline,
            "latency": n.latency_ms,
            "downloadThroughput": n.download_kbps * 1000.0 / 8.0,
            "uploadThroughput": n.upload_kbps * 1000.0 / 8.0,
        }),
        None => json!({
            "offline": false,
            "latency": 0,
            "downloadThroughput": -1,
            "uploadThroughput": -1,
        }),
    }
}

/// Next to the session records, whose directory `sessions_dir` names.
pub fn applied_dir(sessions_dir: &Path) -> PathBuf {
    sessions_dir.with_file_name("emulation-applied")
}

/// `<dir>/<id>`, or `None` for an id that isn't a plain browserContextId.
fn applied_path(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let valid = !session_id.is_empty()
        && session_id.len() <= 64
        && session_id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| applied_dir(sessions_dir).join(session_id))
}

pub async fn applied_revision(session_id: &str) -> Option<u64> {
    let raw = tokio::fs::read_to_string(applied_path(&sessions_dir(), session_id)?)
        .await
        .ok()?;
    raw.trim().parse().ok()
}

pub async fn write_applied(session_id: &str, rev: u64) -> Result<()> {
    let dir = sessions_dir();
    let Some(path) = applied_path(&dir, session_id) else {
        bail!("invalid session id {session_id:?}");
    };
    tokio::fs::create_dir_all(applied_dir(&dir)).await?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    tokio::fs::write(&tmp, rev.to_string()).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

pub async fn remove_applied(sessions_dir: &Path, session_id: &str) {
    if let Some(path) = applied_path(sessions_dir, session_id) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(id: &str) -> InitScript {
        InitScript {
            id: id.into(),
            source: format!("window.{id}=1"),
        }
    }

    #[test]
    fn untouched_tab_gets_nothing_without_emulation() {
        let mut applied = TabApplied::default();
        assert!(plan(&Emulation::default(), &[], &mut applied).is_empty());
        assert!(plan(&Emulation::default(), &[], &mut applied).is_empty());
    }

    #[test]
    fn changes_send_only_what_changed() {
        let mut applied = TabApplied::default();
        let slow = Emulation {
            cpu_throttling_rate: 4.0,
            network: preset("slow4g"),
        };
        let cmds = plan(&slow, &[], &mut applied);
        assert_eq!(cmds.len(), 2);
        assert!(plan(&slow, &[], &mut applied).is_empty());

        let cpu_only = Emulation {
            cpu_throttling_rate: 4.0,
            network: None,
        };
        let cmds = plan(&cpu_only, &[], &mut applied);
        assert_eq!(
            cmds,
            vec![Cmd::Send(
                "Network.emulateNetworkConditions",
                json!({ "offline": false, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 })
            )]
        );
        let off = plan(&Emulation::default(), &[], &mut applied);
        assert_eq!(
            off,
            vec![Cmd::Send(
                "Emulation.setCPUThrottlingRate",
                json!({ "rate": 1.0 })
            )]
        );
    }

    #[test]
    fn scripts_enable_page_once_and_remove_by_identifier() {
        let mut applied = TabApplied::default();
        let cmds = plan(
            &Emulation::default(),
            &[script("a"), script("b")],
            &mut applied,
        );
        assert_eq!(cmds[0], Cmd::Send("Page.enable", json!({})));
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::AddScript(..)))
                .count(),
            2
        );
        applied.scripts.insert("a".into(), Some("7".into()));
        applied.scripts.insert("b".into(), Some("8".into()));
        let cmds = plan(&Emulation::default(), &[script("b")], &mut applied);
        assert_eq!(
            cmds,
            vec![Cmd::Send(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({ "identifier": "7" })
            )]
        );
    }

    #[test]
    fn network_argument_parses_presets_and_custom_values() {
        assert_eq!(parse_network(&json!("off")).unwrap(), None);
        assert_eq!(
            parse_network(&json!("slow4g")).unwrap().unwrap().latency_ms,
            562.5
        );
        assert!(parse_network(&json!("offline")).unwrap().unwrap().offline);
        assert!(parse_network(&json!("2g")).is_err());
        let custom =
            parse_network(&json!({ "latencyMs": 150, "downloadKbps": 1600, "uploadKbps": 750 }))
                .unwrap()
                .unwrap();
        assert_eq!(
            network_params(Some(&custom))["downloadThroughput"],
            200000.0
        );
        assert!(
            parse_network(&json!({ "latencyMs": 1, "downloadKbps": 0, "uploadKbps": 1 })).is_err()
        );
    }
}
