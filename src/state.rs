//! Per-session metadata persisted to disk so the reaper can find idle
//! BrowserContexts after the MCP subprocess has been recycled, and so the
//! listener can mask a session's popups with its UA.
//!
//! One file per session (`$STATE_DIR/sessions/<id>.json`). Several MCP
//! processes run at once (one per client), and each only ever writes the
//! sessions it touched, so none can erase another's records; with a single
//! shared file the last writer used to win. Writes are debounced (1s) and
//! atomic (write to a per-process .tmp + rename) so a flurry of tool calls
//! doesn't thrash the filesystem and a crash mid-write can't truncate a file.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    fs,
    sync::{Mutex, Notify},
};

use crate::sessions::Viewport;
use crate::user_agent::UaOverride;

pub fn sessions_dir() -> PathBuf {
    crate::state_dir().join("sessions")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(rename = "lastUsedAt")]
    pub last_used_at: String,
    #[serde(rename = "userAgent", default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(
        rename = "userAgentMetadata",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user_agent_metadata: Option<serde_json::Value>,
    /// Viewport chosen at `open_browser_session`, applied to every tab of the
    /// session (not just the first) so screenshots and coordinates agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Viewport>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Sessions this process has read or written. Another process's session
    /// is read from disk on first use; only `lastUsedAt` ever changes after a
    /// session opens, so a cached record can't go meaningfully stale.
    cache: HashMap<String, SessionRecord>,
    dirty: HashSet<String>,
}

#[derive(Clone)]
pub struct StateStore {
    dir: PathBuf,
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
    /// Held across snapshot + write so `forget` can't delete a file that a
    /// flush then resurrects from an older snapshot.
    write_lock: Arc<Mutex<()>>,
}

impl StateStore {
    pub async fn load(dir: PathBuf) -> Result<Self> {
        migrate_legacy(&dir).await;
        let store = Self {
            dir,
            inner: Arc::new(Mutex::new(Inner::default())),
            notify: Arc::new(Notify::new()),
            write_lock: Arc::new(Mutex::new(())),
        };
        store.spawn_flusher();
        Ok(store)
    }

    fn spawn_flusher(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                store.notify.notified().await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(err) = store.flush_now().await {
                    tracing::warn!(dir = %store.dir.display(), error = %err, "state flush failed; will retry");
                    store.notify.notify_one();
                }
            }
        });
    }

    /// Run `f` on the session's record (loaded from disk, or new), mark it for
    /// the next flush.
    async fn update(&self, session_id: &str, f: impl FnOnce(&mut SessionRecord)) {
        let mut guard = self.inner.lock().await;
        if !guard.cache.contains_key(session_id) {
            let rec = read_session(&self.dir, session_id)
                .await
                .unwrap_or_default();
            guard.cache.insert(session_id.to_string(), rec);
        }
        if let Some(rec) = guard.cache.get_mut(session_id) {
            f(rec);
            if rec.last_used_at.is_empty() {
                rec.last_used_at = now_rfc3339();
            }
        }
        guard.dirty.insert(session_id.to_string());
        self.notify.notify_one();
    }

    async fn get(&self, session_id: &str) -> Option<SessionRecord> {
        if let Some(rec) = self.inner.lock().await.cache.get(session_id) {
            return Some(rec.clone());
        }
        let rec = read_session(&self.dir, session_id).await?;
        self.inner
            .lock()
            .await
            .cache
            .insert(session_id.to_string(), rec.clone());
        Some(rec)
    }

    pub async fn touch(&self, session_id: &str) {
        let now = now_rfc3339();
        self.update(session_id, |rec| rec.last_used_at = now).await;
    }

    pub async fn set_user_agent_override(&self, session_id: &str, override_: &UaOverride) {
        self.update(session_id, |rec| {
            rec.user_agent = Some(override_.user_agent.clone());
            rec.user_agent_metadata = Some(override_.metadata.clone());
        })
        .await;
    }

    pub async fn set_viewport(&self, session_id: &str, viewport: Viewport) {
        self.update(session_id, |rec| rec.viewport = Some(viewport))
            .await;
    }

    pub async fn viewport(&self, session_id: &str) -> Option<Viewport> {
        self.get(session_id).await?.viewport
    }

    pub async fn user_agent_override(&self, session_id: &str) -> Option<UaOverride> {
        let rec = self.get(session_id).await?;
        Some(UaOverride {
            user_agent: rec.user_agent?,
            metadata: rec.user_agent_metadata?,
        })
    }

    pub async fn forget(&self, session_id: &str) {
        let _w = self.write_lock.lock().await;
        {
            let mut guard = self.inner.lock().await;
            guard.cache.remove(session_id);
            guard.dirty.remove(session_id);
        }
        if let Err(err) = remove_session(&self.dir, session_id).await {
            tracing::warn!(session = session_id, error = %err, "removing session file failed");
        }
    }

    pub async fn flush_now(&self) -> Result<()> {
        let _w = self.write_lock.lock().await;
        let batch: Vec<(String, SessionRecord)> = {
            let mut guard = self.inner.lock().await;
            let ids: Vec<String> = guard.dirty.drain().collect();
            ids.into_iter()
                .filter_map(|id| guard.cache.get(&id).cloned().map(|rec| (id, rec)))
                .collect()
        };
        let mut first_err = None;
        for (id, rec) in batch {
            if let Err(err) = write_session(&self.dir, &id, &rec).await {
                // Retry on the next flush.
                self.inner.lock().await.dirty.insert(id);
                first_err.get_or_insert(err);
            }
        }
        first_err.map_or(Ok(()), Err)
    }
}

/// `<dir>/<id>.json`, or `None` for an id that isn't a plain browserContextId
/// (hex), so a crafted id can't name a path outside `dir`.
fn session_file(dir: &Path, session_id: &str) -> Option<PathBuf> {
    let valid = !session_id.is_empty()
        && session_id.len() <= 64
        && session_id.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then(|| dir.join(format!("{session_id}.json")))
}

/// One session's record, if it has a readable file.
pub async fn read_session(dir: &Path, session_id: &str) -> Option<SessionRecord> {
    let raw = fs::read_to_string(session_file(dir, session_id)?)
        .await
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// Every session with a file (reaper). Unreadable files are skipped.
pub async fn read_sessions(dir: &Path) -> Result<HashMap<String, SessionRecord>> {
    let mut out = HashMap::new();
    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Some(rec) = read_session(dir, id).await {
            out.insert(id.to_string(), rec);
        }
    }
    Ok(out)
}

pub async fn write_session(dir: &Path, session_id: &str, rec: &SessionRecord) -> Result<()> {
    let Some(file) = session_file(dir, session_id) else {
        anyhow::bail!("invalid session id {session_id:?}");
    };
    let bytes = serde_json::to_vec_pretty(rec).context("serializing session")?;
    atomic_write(&file, &bytes).await
}

pub async fn remove_session(dir: &Path, session_id: &str) -> Result<()> {
    let Some(file) = session_file(dir, session_id) else {
        return Ok(());
    };
    match fs::remove_file(&file).await {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", file.display()))
        }
        _ => Ok(()),
    }
}

/// Split a pre-per-session `state.json` (next to `dir`) into session files,
/// keeping any file that already exists, then retire it. Idempotent, so every
/// role runs it at startup and processes starting together can't conflict.
pub async fn migrate_legacy(dir: &Path) {
    #[derive(Deserialize)]
    struct Legacy {
        #[serde(default)]
        sessions: HashMap<String, SessionRecord>,
    }
    let legacy = dir.with_file_name("state.json");
    let Ok(raw) = fs::read_to_string(&legacy).await else {
        return;
    };
    let sessions = serde_json::from_str::<Legacy>(&raw)
        .map(|l| l.sessions)
        .unwrap_or_default();
    for (id, rec) in &sessions {
        if read_session(dir, id).await.is_none() {
            if let Err(err) = write_session(dir, id, rec).await {
                tracing::warn!(session = %id, error = %err, "migrating session record failed");
                return;
            }
        }
    }
    let _ = fs::rename(&legacy, legacy.with_extension("json.migrated")).await;
    tracing::info!(
        count = sessions.len(),
        "migrated state.json to per-session files"
    );
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::new())
}

async fn atomic_write(file: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("mkdir -p {}", dir.display()))?;
    }
    // Per-process temp name: two processes flushing the same session must not
    // write through one temp file.
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, file)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bsm-state-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // The bug this layout fixes: two stores (two MCP processes) each flush,
    // and neither may erase the other's session.
    #[tokio::test]
    async fn concurrent_stores_keep_each_others_sessions() {
        let root = tmpdir();
        let dir = root.join("sessions");
        let a = StateStore::load(dir.clone()).await.unwrap();
        let b = StateStore::load(dir.clone()).await.unwrap();
        a.set_viewport(
            "AAA111",
            Viewport {
                width: 800,
                height: 600,
            },
        )
        .await;
        b.touch("BBB222").await;
        a.flush_now().await.unwrap();
        b.flush_now().await.unwrap();
        a.touch("AAA111").await;
        a.flush_now().await.unwrap();
        let all = read_sessions(&dir).await.unwrap();
        assert!(
            all.contains_key("AAA111") && all.contains_key("BBB222"),
            "{all:?}"
        );
        // b sees a's session from disk.
        assert_eq!(b.viewport("AAA111").await.unwrap().width, 800);
        b.forget("AAA111").await;
        assert!(!read_sessions(&dir).await.unwrap().contains_key("AAA111"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn migrates_legacy_state_file() {
        let root = tmpdir();
        std::fs::write(
            root.join("state.json"),
            r#"{"sessions":{"OLD1":{"lastUsedAt":"2026-10-01T00:00:00Z","userAgent":"UA"}}}"#,
        )
        .unwrap();
        let dir = root.join("sessions");
        let s = StateStore::load(dir.clone()).await.unwrap();
        assert!(
            s.get("OLD1")
                .await
                .is_some_and(|r| r.user_agent.as_deref() == Some("UA"))
        );
        assert!(!root.join("state.json").exists());
        assert!(root.join("state.json.migrated").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn session_ids_cannot_escape_the_dir() {
        let d = Path::new("/x/sessions");
        assert!(session_file(d, "../../etc/passwd").is_none());
        assert!(session_file(d, "").is_none());
        assert_eq!(session_file(d, "AB12").unwrap(), d.join("AB12.json"));
    }
}
