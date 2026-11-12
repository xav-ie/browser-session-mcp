//! Our isolated-world scripts, run inside frames page scripts can't reach.
//!
//! The action helpers find elements with JS that walks open shadow roots and
//! same-origin iframes. A cross-origin iframe (an application form embedded
//! from another site) is closed to that JS: its `contentDocument` is `null`
//! even from a world with universal access. So such frames are reached over
//! CDP instead: an isolated world is made in the frame itself, through the
//! tab's own session when Chrome keeps the frame in the tab's process, or
//! through the frame's own target when site isolation gives it a process of
//! its own.
//!
//! chromiumoxide only sends commands on a page's own session, so this holds a
//! raw connection (like `attach_guard` and `child_capture`) and attaches to
//! the tab and to each out-of-process frame itself.
//!
//! Scripts in a frame see coordinates in that frame's viewport. Each call
//! sets `globalThis.__bsBase` to the frame's offset in the top viewport, which
//! [`crate::jev::DEEP_FIND_JS`] adds, so points come back in the coordinates
//! input events use; Chrome routes those events into the frame itself.
//!
//! A tool call that finds its target in a frame keeps working there: see
//! [`scope`], [`current`] and `iso_eval_function` in the server.
use anyhow::{Context, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use chromiumoxide::Page;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Longest wait for one command's answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one tool call reuses the frames it listed.
const FRAMES_FRESH: Duration = Duration::from_millis(1500);

/// Deepest frame nesting followed.
const MAX_DEPTH: usize = 4;

/// A CDP error answer (as opposed to a lost connection or a timeout): the
/// command reached Chrome and was refused.
#[derive(Debug)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

struct Conn {
    tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
}

static CONN: Lazy<tokio::sync::Mutex<Option<Arc<Conn>>>> = Lazy::new(Default::default);

/// Sessions this connection attached, by target id.
static SESSIONS: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(Default::default);

/// Isolated worlds made through this connection, by `target:frame`.
static WORLDS: Lazy<Mutex<HashMap<String, World>>> = Lazy::new(Default::default);

#[derive(Clone)]
struct World {
    context_id: i64,
    marker: String,
}

async fn conn() -> Result<Arc<Conn>> {
    let mut slot = CONN.lock().await;
    if let Some(c) = slot.as_ref().filter(|c| c.alive.load(Ordering::SeqCst)) {
        return Ok(c.clone());
    }
    let browser_url =
        std::env::var("BROWSER_URL").map_err(|_| anyhow!("BROWSER_URL is not set"))?;
    let ws_url = crate::chrome::resolve_ws_endpoint(&browser_url).await?;
    let (ws, _) = async_tungstenite::tokio::connect_async(ws_url)
        .await
        .context("frames connect")?;
    SESSIONS.lock().expect("sessions lock").clear();
    WORLDS.lock().expect("worlds lock").clear();
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Default::default();
    let alive = Arc::new(AtomicBool::new(true));
    {
        let alive = alive.clone();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::SeqCst);
        });
    }
    {
        let (pending, alive) = (pending.clone(), alive.clone());
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Ok(text) = msg.to_text() else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                        let _ = tx.send(v);
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            pending.lock().expect("pending lock").clear();
        });
    }
    let c = Arc::new(Conn {
        tx,
        pending,
        next: AtomicU64::new(0),
        alive,
    });
    *slot = Some(c.clone());
    Ok(c)
}

/// One CDP command on this module's connection, on `session` or the browser.
async fn call(method: &str, params: Value, session: Option<&str>) -> Result<Value> {
    let c = conn().await?;
    let id = c.next.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = oneshot::channel();
    c.pending.lock().expect("pending lock").insert(id, tx);
    let mut msg = json!({ "id": id, "method": method, "params": params });
    if let Some(s) = session {
        msg["sessionId"] = json!(s);
    }
    if c.tx.send(Message::text(msg.to_string())).is_err() {
        c.pending.lock().expect("pending lock").remove(&id);
        return Err(anyhow!("the frames connection closed"));
    }
    let v = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => return Err(anyhow!("the frames connection closed")),
        Err(_) => {
            c.pending.lock().expect("pending lock").remove(&id);
            return Err(anyhow!("{method} timed out"));
        }
    };
    if let Some(err) = v.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("CDP error");
        return Err(ProtocolError(format!("{method}: {message}")).into());
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// The session attached to `target` (a tab or an out-of-process frame).
async fn session(target: &str) -> Result<String> {
    if let Some(s) = SESSIONS.lock().expect("sessions lock").get(target) {
        return Ok(s.clone());
    }
    let r = call(
        "Target.attachToTarget",
        json!({ "targetId": target, "flatten": true }),
        None,
    )
    .await?;
    let s = r
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Target.attachToTarget returned no session"))?
        .to_string();
    SESSIONS
        .lock()
        .expect("sessions lock")
        .insert(target.to_string(), s.clone());
    Ok(s)
}

/// A command on `target`'s session, attaching again once if Chrome forgot
/// the session (the target navigated to another process, say).
async fn on_target(target: &str, method: &str, params: Value) -> Result<Value> {
    let s = session(target).await?;
    match call(method, params.clone(), Some(&s)).await {
        Err(e)
            if e.downcast_ref::<ProtocolError>()
                .is_some_and(|p| p.0.contains("ession")) =>
        {
            SESSIONS.lock().expect("sessions lock").remove(target);
            let s = session(target).await?;
            call(method, params, Some(&s)).await
        }
        other => other,
    }
}

/// One `Runtime.callFunctionOn`; the inner result is the JS outcome.
async fn call_function(target: &str, params: Value) -> Result<Result<Value>> {
    let r = on_target(target, "Runtime.callFunctionOn", params).await?;
    if let Some(exc) = r.get("exceptionDetails") {
        let desc = exc
            .pointer("/exception/description")
            .and_then(Value::as_str)
            .or_else(|| exc.get("text").and_then(Value::as_str))
            .unwrap_or("exception");
        let desc: String = desc.lines().take(4).collect::<Vec<_>>().join("\n");
        return Ok(Err(anyhow!("JS exception: {desc}")));
    }
    Ok(Ok(r
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null)))
}

/// Patched into each world: records that a call scrolled, which can move
/// the frame on the page (see [`eval`]).
const WORLD_SETUP: &str = "var o = Element.prototype.scrollIntoView; \
  Element.prototype.scrollIntoView = function() { globalThis.__bsScrolled = true; return o.apply(this, arguments); };";

/// Run `decl` in our isolated world in `frame_id` (kept per document, behind
/// a random marker, like the server's own worlds).
async fn world_call(target: &str, frame_id: &str, decl: &str) -> Result<Value> {
    let key = format!("{target}:{frame_id}");
    let cached = WORLDS.lock().expect("worlds lock").get(&key).cloned();
    if let Some(w) = cached {
        match guarded(target, &w, decl).await {
            Ok(Some(outcome)) => return outcome,
            Ok(None) => {}
            Err(e) if e.downcast_ref::<ProtocolError>().is_some() => {}
            Err(e) => return Err(e),
        }
    }
    let created = on_target(
        target,
        "Page.createIsolatedWorld",
        json!({ "frameId": frame_id, "grantUniveralAccess": true }),
    )
    .await?;
    let context_id = created
        .get("executionContextId")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("Page.createIsolatedWorld returned no context"))?;
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("getrandom: {e}"))?;
    let w = World {
        context_id,
        marker: buf.iter().map(|b| format!("{b:02x}")).collect(),
    };
    let mark = format!(
        "function() {{ globalThis._{} = true; {WORLD_SETUP} return true; }}",
        w.marker
    );
    call_function(
        target,
        json!({ "functionDeclaration": mark, "executionContextId": w.context_id, "returnByValue": true }),
    )
    .await??;
    {
        let mut worlds = WORLDS.lock().expect("worlds lock");
        if worlds.len() >= 1024 {
            worlds.clear();
        }
        worlds.insert(key, w.clone());
    }
    guarded(target, &w, decl)
        .await?
        .ok_or_else(|| anyhow!("the new isolated world lost its marker"))?
}

const STALE: &str = "__bsw_stale__";

async fn guarded(target: &str, w: &World, decl: &str) -> Result<Option<Result<Value>>> {
    let f = format!(
        "function() {{ if (globalThis._{m} !== true) return '{STALE}';\n\
         return (\n{decl}\n).apply(this, arguments); }}",
        m = w.marker
    );
    let outcome = call_function(
        target,
        json!({
            "functionDeclaration": f,
            "executionContextId": w.context_id,
            "awaitPromise": true,
            "returnByValue": true,
        }),
    )
    .await?;
    Ok(match outcome {
        Ok(Value::String(s)) if s == STALE => None,
        o => Some(o),
    })
}

/// One step from a frame to a frame inside it.
#[derive(Clone, Debug)]
struct Link {
    /// The target whose session runs the outer frame.
    target: String,
    outer: String,
    inner: String,
}

/// A frame below the tab's top document.
#[derive(Clone, Debug)]
pub struct Frame {
    /// The target whose session runs this frame: the tab's, or the frame's
    /// own when it has a process of its own.
    target: String,
    frame_id: String,
    pub url: String,
    /// The steps from the top document down to this frame.
    links: Vec<Link>,
    /// Whether scripts in the frame around it can't reach into it.
    pub cross: bool,
    /// The `<iframe>`'s node in the frame around it, matched against the
    /// accessibility tree there.
    pub owner: Option<i64>,
    /// The frame around it, as `target:frame`.
    pub outer_key: String,
}

impl Frame {
    /// `target:frame`, naming this frame in [`Frame::outer_key`]s.
    pub fn key(&self) -> String {
        format!("{}:{}", self.target, self.frame_id)
    }

    /// Its host, for descriptions ("in the frame from boards.example.com").
    pub fn host(&self) -> String {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| self.url.chars().take(60).collect())
    }
}

/// The tab's frame tree, flattened: (frame id, parent id, url, origin).
fn flatten_tree(
    node: &Value,
    parent: Option<&str>,
    out: &mut Vec<(String, Option<String>, String, String)>,
) {
    let frame = &node["frame"];
    let id = frame["id"].as_str().unwrap_or_default().to_string();
    out.push((
        id.clone(),
        parent.map(str::to_string),
        frame["url"].as_str().unwrap_or_default().to_string(),
        frame["securityOrigin"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    ));
    if let Some(kids) = node["childFrames"].as_array() {
        for k in kids {
            flatten_tree(k, Some(&id), out);
        }
    }
}

/// Every frame below `page`'s top document, outer frames first.
pub async fn list(page: &Page) -> Result<Vec<Frame>> {
    let tab = page.target_id().as_ref().to_string();
    let targets = call("Target.getTargets", json!({}), None).await?;
    let iframes: Vec<(String, String, String)> = targets["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["type"] == "iframe")
        .filter_map(|t| {
            Some((
                t["targetId"].as_str()?.to_string(),
                t["parentFrameId"].as_str()?.to_string(),
                t["url"].as_str().unwrap_or_default().to_string(),
            ))
        })
        .collect();
    let mut out = Vec::new();
    let mut hosts = vec![(tab, Vec::<Link>::new(), 0usize)];
    while let Some((target, links, depth)) = hosts.pop() {
        let tree = on_target(&target, "Page.getFrameTree", json!({})).await?;
        let mut flat = Vec::new();
        flatten_tree(&tree["frameTree"], None, &mut flat);
        let mut chains: HashMap<String, Vec<Link>> = HashMap::new();
        let origins: HashMap<String, String> = flat
            .iter()
            .map(|(id, _, _, origin)| (id.clone(), origin.clone()))
            .collect();
        for (id, parent, url, origin) in &flat {
            let Some(parent) = parent else {
                chains.insert(id.clone(), links.clone());
                continue;
            };
            let mut chain = chains.get(parent).cloned().unwrap_or_default();
            chain.push(Link {
                target: target.clone(),
                outer: parent.clone(),
                inner: id.clone(),
            });
            chains.insert(id.clone(), chain.clone());
            let outer_origin = origins.get(parent).map(String::as_str).unwrap_or_default();
            out.push(Frame {
                target: target.clone(),
                frame_id: id.clone(),
                url: url.clone(),
                links: chain,
                cross: origin != outer_origin || origin == "null" || origin.is_empty(),
                owner: None,
                outer_key: format!("{target}:{parent}"),
            });
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        for (child, parent, url) in &iframes {
            let Some(chain) = chains.get(parent) else {
                continue;
            };
            let mut chain = chain.clone();
            chain.push(Link {
                target: target.clone(),
                outer: parent.clone(),
                inner: child.clone(),
            });
            out.push(Frame {
                target: child.clone(),
                frame_id: child.clone(),
                url: url.clone(),
                links: chain.clone(),
                cross: true,
                owner: None,
                outer_key: format!("{target}:{parent}"),
            });
            hosts.push((child.clone(), chain, depth + 1));
        }
    }
    for f in &mut out {
        if let Some(last) = f.links.last() {
            f.owner = on_target(
                &last.target,
                "DOM.getFrameOwner",
                json!({ "frameId": last.inner }),
            )
            .await
            .ok()
            .and_then(|o| o["backendNodeId"].as_i64());
        }
    }
    Ok(out)
}

/// The `<iframe>`'s content box in its outer frame's viewport, `None` when
/// it has no size.
async fn link_box(link: &Link) -> Result<Option<(f64, f64)>> {
    let owner = on_target(
        &link.target,
        "DOM.getFrameOwner",
        json!({ "frameId": link.inner }),
    )
    .await?;
    let node = owner["backendNodeId"]
        .as_i64()
        .ok_or_else(|| anyhow!("DOM.getFrameOwner returned no node"))?;
    // A world in the outer frame, so the owner resolves where our JS runs.
    world_call(&link.target, &link.outer, "function() { return true; }").await?;
    let key = format!("{}:{}", link.target, link.outer);
    let ctx = WORLDS
        .lock()
        .expect("worlds lock")
        .get(&key)
        .map(|w| w.context_id)
        .ok_or_else(|| anyhow!("no world in the outer frame"))?;
    let resolved = on_target(
        &link.target,
        "DOM.resolveNode",
        json!({ "backendNodeId": node, "executionContextId": ctx }),
    )
    .await?;
    let object = resolved
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DOM.resolveNode returned no object"))?
        .to_string();
    let v = call_function(
        &link.target,
        json!({
            "objectId": object,
            "functionDeclaration": "function() { var r = this.getBoundingClientRect(), s = getComputedStyle(this); \
               if (!r.width || !r.height) return null; \
               return [r.left + this.clientLeft + parseFloat(s.paddingLeft || 0), r.top + this.clientTop + parseFloat(s.paddingTop || 0)]; }",
            "returnByValue": true,
        }),
    )
    .await;
    let _ = on_target(
        &link.target,
        "Runtime.releaseObject",
        json!({ "objectId": object }),
    )
    .await;
    let v = v??;
    Ok(v.as_array()
        .filter(|a| a.len() == 2)
        .map(|a| (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0))))
}

/// Where `frame`'s viewport sits in the top viewport, `None` when it (or a
/// frame around it) has no size.
async fn base_of(frame: &Frame) -> Result<Option<(f64, f64)>> {
    let (mut x, mut y) = (0.0, 0.0);
    for link in &frame.links {
        match link_box(link).await? {
            Some((dx, dy)) => {
                x += dx;
                y += dy;
            }
            None => return Ok(None),
        }
    }
    Ok(Some((x, y)))
}

/// Run a function declaration in our isolated world in `frame`, with
/// `__bsBase` set to the frame's offset. A call that scrolls can move the
/// frame itself on the page (an embedded form taller than the screen), and
/// then the points it computed are off by that move: such a call runs once
/// more with the new offset. Only the helpers that scroll an element into
/// view and measure it scroll, and they are safe to repeat.
pub async fn eval(frame: &Frame, decl: &str) -> Result<Value> {
    let base = base_of(frame).await?.unwrap_or((0.0, 0.0));
    let run = |b: (f64, f64)| {
        format!(
            "function() {{ globalThis.__bsBase = {{ x: {}, y: {} }}; globalThis.__bsScrolled = false;\n\
             return (\n{decl}\n).apply(this, arguments); }}",
            b.0, b.1
        )
    };
    let v = world_call(&frame.target, &frame.frame_id, &run(base)).await?;
    let scrolled = world_call(
        &frame.target,
        &frame.frame_id,
        "function() { return globalThis.__bsScrolled === true; }",
    )
    .await
    .ok()
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
    if scrolled {
        if let Some(now) = settled_base(frame).await {
            if (now.0 - base.0).abs() > 0.5 || (now.1 - base.1).abs() > 0.5 {
                return world_call(&frame.target, &frame.frame_id, &run(now)).await;
            }
        }
    }
    Ok(v)
}

/// The frame's offset once a scroll has finished moving it. A frame in a
/// process of its own scrolls the page around it a moment later, through
/// the browser, so the offset is read until two reads agree.
async fn settled_base(frame: &Frame) -> Option<(f64, f64)> {
    let mut last = base_of(frame).await.ok().flatten()?;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let now = base_of(frame).await.ok().flatten()?;
        if (now.0 - last.0).abs() < 0.5 && (now.1 - last.1).abs() < 0.5 {
            return Some(now);
        }
        last = now;
    }
    Some(last)
}

/// The accessibility tree of `frame` (its own nodes, without frames inside).
pub async fn ax_nodes(frame: &Frame) -> Result<Value> {
    on_target(
        &frame.target,
        "Accessibility.getFullAXTree",
        json!({ "frameId": frame.frame_id }),
    )
    .await
}

/// What one tool call knows about frames.
#[derive(Default)]
struct CallState {
    /// The frame the call's target is in; `None` is the top document.
    current: Option<Frame>,
    /// The tab's frames, listed lately.
    listed: Option<(String, Instant, Vec<Frame>)>,
}

tokio::task_local! {
    static STATE: RefCell<CallState>;
}

/// Run one tool call with its own frame state.
pub async fn scope<F: Future>(f: F) -> F::Output {
    STATE.scope(RefCell::new(CallState::default()), f).await
}

/// The frame this tool call works in, if its target is in one.
pub fn current() -> Option<Frame> {
    STATE
        .try_with(|s| s.borrow().current.clone())
        .ok()
        .flatten()
}

/// Work in `frame` (or the top document) for the rest of this tool call.
pub fn set_current(frame: Option<Frame>) {
    let _ = STATE.try_with(|s| s.borrow_mut().current = frame);
}

/// `page`'s frames, reusing a recent listing within this tool call.
pub async fn frames(page: &Page) -> Vec<Frame> {
    let tab = page.target_id().as_ref().to_string();
    let recent = STATE
        .try_with(|s| {
            s.borrow().listed.as_ref().and_then(|(t, at, f)| {
                (t == &tab && at.elapsed() < FRAMES_FRESH).then(|| f.clone())
            })
        })
        .ok()
        .flatten();
    if let Some(f) = recent {
        return f;
    }
    let listed = match list(page).await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "listing frames failed");
            Vec::new()
        }
    };
    let _ = STATE.try_with(|s| {
        s.borrow_mut().listed = Some((tab, Instant::now(), listed.clone()));
    });
    listed
}

/// The frames page scripts can't reach, which only [`eval`] can.
pub async fn closed(page: &Page) -> Vec<Frame> {
    frames(page).await.into_iter().filter(|f| f.cross).collect()
}
