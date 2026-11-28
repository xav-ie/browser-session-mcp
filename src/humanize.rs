//! Input that moves like a person's, so bot detection that watches the
//! mouse and keyboard sees a person. On by default; a session turns it off
//! with `humanize: false` on open_browser_session.
//!
//! The pure generators here (cursor paths, press and typing timings) take a
//! seedable [`Rng`], so tests pin them down. The mouse goes through
//! [`mouse_event`], which every click, move, drag and wheel uses: it keeps
//! the pressed buttons, presses with a real mouse's force, and remembers
//! where the cursor is. The cursor is saved per session (next to the
//! stealth flag), so a new MCP process moves on from where the last left it.
//! No event carries a CDP `timestamp`: Chrome stamps each as it arrives.
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::GetLayoutMetricsParams;
use futures::StreamExt;

/// A small seedable generator (splitmix64): enough randomness for input
/// timing, and the same sequence for the same seed in tests.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    /// Seeded from the OS.
    pub fn fresh() -> Self {
        let mut b = [0u8; 8];
        let seed = match getrandom::fill(&mut b) {
            Ok(()) => u64::from_le_bytes(b),
            Err(_) => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0x5eed, |d| d.as_nanos() as u64),
        };
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn normal(&mut self, mean: f64, sd: f64) -> f64 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        mean + sd * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Log-normal with the given median and σ of its logarithm.
    pub fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.normal(0.0, 1.0)).exp()
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

/// A box in viewport CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// One cursor position on a path, reached `wait_ms` after the previous one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub x: f64,
    pub y: f64,
    pub wait_ms: f64,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
}

/// Where in `r` to click: around its center (SD a sixth of its size), kept
/// inside its middle 70%.
pub fn aim(r: Rect, rng: &mut Rng) -> (f64, f64) {
    let (cx, cy) = r.center();
    let x = rng
        .normal(cx, r.w / 6.0)
        .clamp(cx - 0.35 * r.w, cx + 0.35 * r.w);
    let y = rng
        .normal(cy, r.h / 6.0)
        .clamp(cy - 0.35 * r.h, cy + 0.35 * r.h);
    (x, y)
}

/// Where the cursor plausibly comes from when nothing has moved it yet:
/// just inside the viewport edge nearest `target`, never the corner.
pub fn entry_point(target: (f64, f64), (vw, vh): (f64, f64), rng: &mut Rng) -> (f64, f64) {
    let (tx, ty) = (target.0.clamp(0.0, vw), target.1.clamp(0.0, vh));
    let edges = [tx, vw - tx, ty, vh - ty];
    let nearest = (0..4)
        .min_by(|&a, &b| edges[a].total_cmp(&edges[b]))
        .unwrap_or(0);
    let inset = rng.uniform(4.0, 30.0);
    let mut along =
        |t: f64, len: f64| (t + rng.uniform(-0.25, 0.25) * len).clamp(len * 0.05, len * 0.95);
    match nearest {
        0 => (inset, along(ty, vh)),
        1 => (vw - inset, along(ty, vh)),
        2 => (along(tx, vw), inset),
        _ => (along(tx, vw), vh - inset),
    }
}

/// Minimum-jerk progress: how far along a movement a hand is at time `t`
/// (both 0 to 1); slow at the ends, fastest in the middle.
pub fn min_jerk(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (10.0 - 15.0 * t + 6.0 * t * t)
}

fn bezier(p: [(f64, f64); 4], s: f64) -> (f64, f64) {
    let u = 1.0 - s;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * s, 3.0 * u * s * s, s * s * s);
    (
        a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0,
        a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1,
    )
}

/// Event gaps of 7 to 17 ms adding up to `total_ms`, between `min` and
/// `max` of them.
fn gaps(total_ms: f64, min: usize, max: usize, rng: &mut Rng) -> Vec<f64> {
    let mut out = Vec::new();
    let mut sum = 0.0;
    while sum < total_ms {
        let g = rng.uniform(7.0, 17.0);
        out.push(g);
        sum += g;
    }
    let n = out.len().clamp(min, max);
    out.resize_with(n, || rng.uniform(7.0, 17.0));
    let scale = total_ms / out.iter().sum::<f64>();
    out.iter_mut().for_each(|g| *g *= scale);
    out
}

/// One curved stroke from `from` to `to` over `total_ms`: a cubic Bezier
/// bowed to a random side by `bend` of the distance, walked with
/// minimum-jerk timing, the middle points off by up to 0.4 px.
fn stroke(
    from: (f64, f64),
    to: (f64, f64),
    total_ms: f64,
    bend: (f64, f64),
    points: (usize, usize),
    rng: &mut Rng,
) -> Vec<Step> {
    let d = dist(from, to).max(1e-9);
    let (ux, uy) = ((to.0 - from.0) / d, (to.1 - from.1) / d);
    let side = if rng.chance(0.5) { 1.0 } else { -1.0 };
    let mut control = |along: (f64, f64)| {
        let a = rng.uniform(along.0, along.1) * d;
        let off = side * rng.uniform(bend.0, bend.1) * d;
        (from.0 + ux * a - uy * off, from.1 + uy * a + ux * off)
    };
    let curve = [from, control((0.2, 0.4)), control((0.6, 0.8)), to];
    let waits = gaps(total_ms, points.0, points.1, rng);
    let n = waits.len();
    let mut elapsed = 0.0;
    waits
        .iter()
        .enumerate()
        .map(|(i, &wait)| {
            elapsed += wait;
            let (mut x, mut y) = bezier(curve, min_jerk(elapsed / total_ms));
            if i + 1 == n {
                (x, y) = to;
            } else {
                x += rng.uniform(-0.4, 0.4);
                y += rng.uniform(-0.4, 0.4);
            }
            Step {
                x,
                y,
                wait_ms: wait,
            }
        })
        .collect()
}

/// How long a move of `d` px to a target `w` px wide takes (Fitts' law,
/// with some spread), in ms.
pub fn move_duration(d: f64, w: f64, rng: &mut Rng) -> f64 {
    ((80.0 + 70.0 * (d / w.max(1.0) + 1.0).log2()) * rng.uniform(0.85, 1.15)).clamp(120.0, 450.0)
}

/// The cursor's path from `from` to `to`, a target `w` px wide: one
/// stroke, overshooting a small far target and correcting, then a settle
/// of a pixel or so. Empty when the cursor is already within 3 px.
pub fn mouse_path(from: (f64, f64), to: (f64, f64), w: f64, rng: &mut Rng) -> Vec<Step> {
    let d = dist(from, to);
    if d < 3.0 {
        return Vec::new();
    }
    let total = move_duration(d, w, rng);
    let overshoot = d > 500.0 && w < 60.0;
    let mut steps = if overshoot {
        let k = rng.uniform(0.04, 0.12);
        let past = (to.0 + (to.0 - from.0) * k, to.1 + (to.1 - from.1) * k);
        let mut s = stroke(from, past, total, (0.1, 0.3), (8, 40), rng);
        let back = rng.uniform(60.0, 120.0);
        s.extend(stroke(past, to, back, (0.0, 0.1), (4, 12), rng));
        s
    } else {
        stroke(from, to, total, (0.1, 0.3), (8, 40), rng)
    };
    let settles = if rng.chance(0.5) { 1 } else { 2 };
    let settle_ms = rng.uniform(20.0, 60.0);
    for _ in 0..settles {
        let (r, a) = (
            rng.uniform(0.3, 1.5),
            rng.uniform(0.0, std::f64::consts::TAU),
        );
        steps.push(Step {
            x: to.0 + r * a.cos(),
            y: to.1 + r * a.sin(),
            wait_ms: settle_ms / settles as f64,
        });
    }
    steps
}

/// The pause between arriving on a target and pressing, in ms.
pub fn press_pause(rng: &mut Rng) -> f64 {
    rng.log_normal(65.0, 0.3).clamp(40.0, 120.0)
}

/// How long a mouse button or finger stays down for a click or tap, in ms.
pub fn hold(rng: &mut Rng) -> f64 {
    rng.normal(85.0, 20.0).clamp(45.0, 160.0)
}

/// How long a key is held down, in ms.
pub fn key_hold(rng: &mut Rng) -> f64 {
    rng.normal(95.0, 22.0).clamp(50.0, 170.0)
}

/// The gap between one key going down and the next, in ms.
pub fn key_gap(rng: &mut Rng) -> f64 {
    rng.log_normal(110.0, 0.35).clamp(40.0, 450.0)
}

/// The gap between wheel notches, in ms.
pub fn wheel_gap(rng: &mut Rng) -> f64 {
    rng.uniform(30.0, 90.0)
}

/// Text longer than this is pasted rather than typed.
pub const PASTE_OVER: usize = 60;

/// Whether `text` is long enough that a person would paste it.
pub fn should_paste(text: &str) -> bool {
    text.chars().count() > PASTE_OVER
}

/// One character to type: the physical key it is on (`None` for one with
/// no key, inserted as text) and whether it takes Shift.
#[derive(Clone, Debug, PartialEq)]
pub struct KeySpec {
    pub ch: char,
    pub code: Option<&'static str>,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyStep {
    /// Press the `n`th character's key (or insert it).
    Down(usize),
    /// Release the `n`th character's key.
    Up(usize),
    ShiftDown,
    ShiftUp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Keyed {
    pub at_ms: f64,
    pub step: KeyStep,
}

/// When each key goes down and up to type `keys`: holds of about 95 ms,
/// gaps of about 110 ms (longer now and then after a space or punctuation,
/// at least `min_gap_ms`), the next key going down before the last comes
/// up when the gap is shorter than the hold, and Shift held around shifted
/// characters. Sorted by time.
pub fn typing_plan(keys: &[KeySpec], min_gap_ms: f64, rng: &mut Rng) -> Vec<Keyed> {
    let mut out = Vec::new();
    let mut shift = false;
    // (down, up) of the last key, and when each physical key last came up.
    let mut last: Option<(f64, f64)> = None;
    let mut released: Vec<(&'static str, f64)> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let hold = key_hold(rng);
        let mut down = match last {
            None => 0.0,
            Some((prev_down, _)) => {
                let mut gap = key_gap(rng);
                let prev = keys[i - 1].ch;
                if (prev.is_whitespace() || prev.is_ascii_punctuation()) && rng.chance(0.15) {
                    gap += rng.uniform(80.0, 250.0);
                }
                prev_down + gap.max(min_gap_ms)
            }
        };
        // A key can't go down again before it has come up.
        if let Some(code) = k.code {
            if let Some((_, up)) = released.iter().find(|(c, _)| *c == code) {
                down = down.max(up + rng.uniform(15.0, 40.0));
            }
        }
        if k.shift != shift {
            // No rollover across Shift: the last key comes up first.
            let prev_up = last.map_or(0.0, |(_, up)| up);
            let at = if last.is_none() {
                0.0
            } else {
                prev_up + rng.uniform(10.0, 50.0)
            };
            out.push(Keyed {
                at_ms: at,
                step: if k.shift {
                    KeyStep::ShiftDown
                } else {
                    KeyStep::ShiftUp
                },
            });
            down = down.max(at + rng.uniform(30.0, 70.0));
            shift = k.shift;
        }
        let up = down + hold;
        out.push(Keyed {
            at_ms: down,
            step: KeyStep::Down(i),
        });
        out.push(Keyed {
            at_ms: up,
            step: KeyStep::Up(i),
        });
        if let Some(code) = k.code {
            released.retain(|(c, _)| *c != code);
            released.push((code, up));
        }
        last = Some((down, up));
    }
    if shift {
        let at = last.map_or(0.0, |(_, up)| up) + rng.uniform(10.0, 40.0);
        out.push(Keyed {
            at_ms: at,
            step: KeyStep::ShiftUp,
        });
    }
    out.sort_by(|a, b| a.at_ms.total_cmp(&b.at_ms));
    out
}

/// A finger's path for a swipe: straight, at an even pace with jittered
/// gaps of about a frame, the points off by up to half a pixel.
pub fn swipe_path(from: (f64, f64), to: (f64, f64), duration_ms: f64, rng: &mut Rng) -> Vec<Step> {
    let total = duration_ms.max(50.0) * rng.uniform(0.9, 1.1);
    let n = ((total / 16.0).round() as usize).clamp(5, 120);
    let mut waits: Vec<f64> = (0..n).map(|_| rng.uniform(0.75, 1.25)).collect();
    let scale = total / waits.iter().sum::<f64>();
    waits.iter_mut().for_each(|w| *w *= scale);
    let mut elapsed = 0.0;
    waits
        .iter()
        .enumerate()
        .map(|(i, &wait)| {
            elapsed += wait;
            let t = elapsed / total;
            let (mut x, mut y) = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            if i + 1 == n {
                (x, y) = to;
            } else {
                x += rng.uniform(-0.5, 0.5);
                y += rng.uniform(-0.5, 0.5);
            }
            Step {
                x,
                y,
                wait_ms: wait,
            }
        })
        .collect()
}

// --- per-call state ---

struct State {
    session: Option<String>,
    on: bool,
    cursor: Option<(f64, f64)>,
    saved: Option<(f64, f64)>,
    buttons: i64,
    rng: Rng,
}

tokio::task_local! {
    static STATE: RefCell<State>;
}

thread_local! {
    static LOOSE_RNG: RefCell<Rng> = RefCell::new(Rng::fresh());
}

/// Run one tool call on `session` with its humanize setting and cursor,
/// saving the cursor afterwards when it moved. Outside a scope input is
/// not humanized.
pub async fn scope<F: Future>(session: Option<&str>, f: F) -> F::Output {
    let (on, cursor) = match session {
        Some(id) => (
            crate::takeover::is_humanize(id).await,
            crate::takeover::cursor(id).await,
        ),
        None => (true, None),
    };
    let state = State {
        session: session.map(str::to_string),
        on,
        cursor,
        saved: cursor,
        buttons: 0,
        rng: Rng::fresh(),
    };
    let (out, end) = STATE
        .scope(RefCell::new(state), async {
            let out = f.await;
            let end = STATE.with(|s| {
                let s = s.borrow();
                (s.session.clone(), s.cursor, s.saved)
            });
            (out, end)
        })
        .await;
    if let (Some(id), Some(at)) = (end.0, end.1) {
        if end.2 != Some(at) {
            crate::takeover::set_cursor(&id, at).await;
        }
    }
    out
}

/// Whether this call's input is humanized.
pub fn enabled() -> bool {
    STATE.try_with(|s| s.borrow().on).unwrap_or(false)
}

/// Where the cursor is, when known.
pub fn cursor() -> Option<(f64, f64)> {
    STATE.try_with(|s| s.borrow().cursor).ok().flatten()
}

/// Draw from this call's random generator.
pub fn with_rng<T>(f: impl FnOnce(&mut Rng) -> T) -> T {
    let mut f = Some(f);
    if let Ok(v) = STATE.try_with(|s| (f.take().expect("called once"))(&mut s.borrow_mut().rng)) {
        return v;
    }
    LOOSE_RNG.with(|r| (f.take().expect("called once"))(&mut r.borrow_mut()))
}

/// Sleep `ms` milliseconds.
pub async fn pause(ms: f64) {
    if ms > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
    }
}

// --- the mouse ---

fn button_bit(b: &MouseButton) -> i64 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        MouseButton::Back => 8,
        MouseButton::Forward => 16,
        _ => 0,
    }
}

/// The button a move reports while buttons are held, as Chrome's own input
/// does.
fn held_button(buttons: i64) -> MouseButton {
    if buttons & 1 != 0 {
        MouseButton::Left
    } else if buttons & 2 != 0 {
        MouseButton::Right
    } else if buttons & 4 != 0 {
        MouseButton::Middle
    } else {
        MouseButton::None
    }
}

/// One CDP mouse event at a viewport point: the one way the mouse is
/// driven. Keeps which buttons are held (a press sets its bit, a release
/// clears it, a move reports them), sends a real mouse's force of 0.5
/// while a button is down (Chrome otherwise reports pressure 0 with
/// buttons pressed), and remembers the cursor.
pub async fn mouse_event(
    page: &Page,
    kind: DispatchMouseEventType,
    at: (f64, f64),
    button: MouseButton,
) -> Result<()> {
    page.execute(mouse_params(kind, at, button)?)
        .await
        .context("Input.dispatchMouseEvent")?;
    Ok(())
}

/// The event [`mouse_event`] sends, noting the buttons and cursor it leaves.
fn mouse_params(
    kind: DispatchMouseEventType,
    (x, y): (f64, f64),
    button: MouseButton,
) -> Result<DispatchMouseEventParams> {
    let held = STATE.try_with(|s| s.borrow().buttons).unwrap_or(0);
    let bit = button_bit(&button);
    let (buttons, button, clicks) = match kind {
        DispatchMouseEventType::MousePressed => (held | bit, button, Some(1)),
        DispatchMouseEventType::MouseReleased => (held & !bit, button, Some(1)),
        _ => (held, held_button(held), None),
    };
    let pressing = buttons != 0 && kind != DispatchMouseEventType::MouseReleased;
    let mut params = DispatchMouseEventParams::builder()
        .r#type(kind)
        .x(x)
        .y(y)
        .button(button)
        .buttons(buttons);
    if let Some(n) = clicks {
        params = params.click_count(n);
    }
    if pressing {
        params = params.force(0.5);
    }
    let params = params.build().map_err(|e| anyhow!("mouse event: {e}"))?;
    let _ = STATE.try_with(|s| {
        let mut s = s.borrow_mut();
        s.buttons = buttons;
        s.cursor = Some((x, y));
    });
    Ok(params)
}

/// One wheel notch (or the whole delta, unhumanized) at `at`.
pub async fn wheel(page: &Page, at: (f64, f64), dx: f64, dy: f64) -> Result<()> {
    let held = STATE.try_with(|s| s.borrow().buttons).unwrap_or(0);
    let params = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(at.0)
        .y(at.1)
        .buttons(held)
        .delta_x(dx)
        .delta_y(dy)
        .build()
        .map_err(|e| anyhow!("wheel: {e}"))?;
    page.execute(params)
        .await
        .context("Input.dispatchMouseEvent (wheel)")?;
    let _ = STATE.try_with(|s| s.borrow_mut().cursor = Some(at));
    Ok(())
}

/// The viewport's size in CSS pixels, read without running page script.
pub async fn viewport(page: &Page) -> (f64, f64) {
    match page.execute(GetLayoutMetricsParams::default()).await {
        Ok(m) => {
            let v = &m.result.css_layout_viewport;
            (v.client_width as f64, v.client_height as f64)
        }
        Err(_) => (1280.0, 800.0),
    }
}

/// A re-measure of the target, run near the end of a move: how far it has
/// moved since it was aimed at, or `None` to keep the aim.
pub type Refine = Pin<Box<dyn Future<Output = Option<(f64, f64)>> + Send>>;

/// Move the cursor to `to` on a target `w` px wide. Humanized, it travels
/// a path from where it is (or from the viewport edge when unknown), and
/// `refine` re-measures the target before the last stretch so a target
/// that moved meanwhile is still hit. Returns where the cursor ends up.
pub async fn move_to(
    page: &Page,
    to: (f64, f64),
    w: f64,
    mut refine: Option<Refine>,
) -> Result<(f64, f64)> {
    if !enabled() {
        mouse_event(
            page,
            DispatchMouseEventType::MouseMoved,
            to,
            MouseButton::None,
        )
        .await?;
        return Ok(to);
    }
    let from = match cursor() {
        Some(at) => at,
        None => {
            let vp = viewport(page).await;
            let at = with_rng(|r| entry_point(to, vp, r));
            mouse_event(
                page,
                DispatchMouseEventType::MouseMoved,
                at,
                MouseButton::None,
            )
            .await?;
            at
        }
    };
    let steps = with_rng(|r| mouse_path(from, to, w, r));
    if steps.is_empty() {
        return Ok(from);
    }
    let check = steps.len().saturating_sub((steps.len() / 4).max(3));
    let mut shift = (0.0, 0.0);
    let mut at = from;
    // Each point is sent at its time on the path without waiting for Chrome
    // to take the last: Chrome hands the page one move per frame, so waiting
    // would slow the path to the frame rate, and moves a real mouse makes
    // between frames show up in getCoalescedEvents.
    let start = tokio::time::Instant::now();
    let mut due = 0.0;
    let mut sent = futures::stream::FuturesOrdered::new();
    for (i, s) in steps.iter().enumerate() {
        if i == check {
            if let Some(f) = refine.take() {
                if let Some(d) = f.await.filter(|d| d.0.abs() > 0.5 || d.1.abs() > 0.5) {
                    shift = d;
                }
            }
        }
        let k = if i >= check {
            (i + 1 - check) as f64 / (steps.len() - check) as f64
        } else {
            0.0
        };
        at = (s.x + shift.0 * k, s.y + shift.1 * k);
        due += s.wait_ms;
        let deadline = start + Duration::from_secs_f64(due / 1000.0);
        loop {
            tokio::select! {
                Some(r) = sent.next(), if !sent.is_empty() => {
                    let r: Result<(), chromiumoxide::error::CdpError> = r;
                    r.context("Input.dispatchMouseEvent")?;
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let params = mouse_params(DispatchMouseEventType::MouseMoved, at, MouseButton::None)?;
        let page = page.clone();
        sent.push_back(async move { page.execute(params).await.map(|_| ()) });
    }
    while let Some(r) = sent.next().await {
        r.context("Input.dispatchMouseEvent")?;
    }
    Ok(at)
}

/// Click `button` on `to`, a target `w` px wide: move there, then press and
/// release; humanized, with a pause before the press and a held button.
pub async fn click(
    page: &Page,
    to: (f64, f64),
    w: f64,
    button: MouseButton,
    refine: Option<Refine>,
) -> Result<()> {
    let at = move_to(page, to, w, refine).await?;
    let human = enabled();
    if human {
        pause(with_rng(press_pause)).await;
    }
    mouse_event(
        page,
        DispatchMouseEventType::MousePressed,
        at,
        button.clone(),
    )
    .await?;
    if human {
        pause(with_rng(hold)).await;
    }
    mouse_event(page, DispatchMouseEventType::MouseReleased, at, button).await
}

/// Scroll by `dx`, `dy` with the wheel. Humanized, it turns in notches of
/// 100 px a moment apart with the cursor over the page (`at`, or where the
/// cursor is, or a point it moves to); otherwise one event at `at`.
pub async fn scroll(page: &Page, at: Option<(f64, f64)>, dx: f64, dy: f64) -> Result<()> {
    if !enabled() {
        return wheel(page, at.unwrap_or((50.0, 50.0)), dx, dy).await;
    }
    let at = pointer_over_page(page, at).await?;
    let major = dy.abs().max(dx.abs());
    let notches = (major / 100.0).ceil().max(1.0) as usize;
    for i in 0..notches {
        if i > 0 {
            pause(with_rng(wheel_gap)).await;
        }
        // The larger delta turns in 100 px notches, the other spread evenly.
        let part = |total: f64| {
            if total.abs() == major {
                total.signum() * (major - 100.0 * i as f64).min(100.0)
            } else {
                total / notches as f64
            }
        };
        wheel(page, at, part(dx), part(dy)).await?;
    }
    Ok(())
}

/// Where the wheel turns: `at` (moving the cursor there), or the cursor
/// when it is over the page, or a point near the middle it moves to.
pub async fn pointer_over_page(page: &Page, at: Option<(f64, f64)>) -> Result<(f64, f64)> {
    if let Some(p) = at {
        return move_to(page, p, 40.0, None).await;
    }
    let (vw, vh) = viewport(page).await;
    if let Some(c) = cursor().filter(|c| c.0 >= 0.0 && c.1 >= 0.0 && c.0 < vw && c.1 < vh) {
        return Ok(c);
    }
    let to = with_rng(|r| (vw * r.uniform(0.3, 0.7), vh * r.uniform(0.3, 0.7)));
    move_to(page, to, 200.0, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeySpec> {
        text.chars()
            .map(|ch| KeySpec {
                ch,
                code: Some(match ch.to_ascii_lowercase() {
                    'a' => "KeyA",
                    'b' => "KeyB",
                    'l' => "KeyL",
                    'o' => "KeyO",
                    ' ' => "Space",
                    _ => "Other",
                }),
                shift: ch.is_ascii_uppercase() || ch == '!',
            })
            .collect()
    }

    #[test]
    fn same_seed_same_draws() {
        let (mut a, mut b) = (Rng::seeded(7), Rng::seeded(7));
        for _ in 0..100 {
            assert_eq!(a.unit().to_bits(), b.unit().to_bits());
        }
        let mut r = Rng::seeded(1);
        for _ in 0..10_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn aim_stays_in_the_middle_of_the_box() {
        let r = Rect {
            x: 100.0,
            y: 50.0,
            w: 80.0,
            h: 20.0,
        };
        let mut rng = Rng::seeded(3);
        let mut off_center = 0;
        for _ in 0..5_000 {
            let (x, y) = aim(r, &mut rng);
            assert!((112.0..=168.0).contains(&x), "{x}");
            assert!((53.0..=67.0).contains(&y), "{y}");
            off_center += usize::from((x - 140.0).abs() > 1.0);
        }
        assert!(
            off_center > 2_500,
            "aims should spread, not sit on the center"
        );
    }

    #[test]
    fn entry_point_is_inside_near_the_nearest_edge() {
        let mut rng = Rng::seeded(5);
        for _ in 0..1_000 {
            let (x, y) = entry_point((1200.0, 400.0), (1280.0, 800.0), &mut rng);
            assert!(x > 1240.0 && x < 1280.0, "{x}");
            assert!(y > 0.0 && y < 800.0);
            let (x, y) = entry_point((0.0, 0.0), (1280.0, 800.0), &mut rng);
            assert!(x > 0.0 && y > 0.0, "never the corner: {x},{y}");
        }
    }

    #[test]
    fn paths_progress_and_fit_their_budget() {
        let mut rng = Rng::seeded(11);
        for i in 0..2_000 {
            let from = (rng.uniform(0.0, 1280.0), rng.uniform(0.0, 800.0));
            let to = (rng.uniform(0.0, 1280.0), rng.uniform(0.0, 800.0));
            let w = rng.uniform(10.0, 300.0);
            let d = dist(from, to);
            let path = mouse_path(from, to, w, &mut rng);
            if d < 3.0 {
                assert!(path.is_empty());
                continue;
            }
            let overshoot = d > 500.0 && w < 60.0;
            let settle = path
                .iter()
                .rev()
                .take_while(|s| dist((s.x, s.y), to) <= 1.5 + 1e-9)
                .count();
            assert!(settle >= 1, "case {i}: ends settling on the target");
            let total: f64 = path.iter().map(|s| s.wait_ms).sum();
            let max = 450.0 * 1.0001 + if overshoot { 120.0 } else { 0.0 } + 60.0;
            assert!((120.0..=max).contains(&total), "case {i}: {total} ms");
            assert!(path.iter().all(|s| s.wait_ms > 0.0 && s.wait_ms < 60.0));
            assert!(
                (9..=54).contains(&path.len()),
                "case {i}: {} points",
                path.len()
            );
            // Nothing lands far outside the box from and to span, plus the bow.
            for s in &path {
                assert!(dist((s.x, s.y), from) <= d * 1.6 + 2.0, "case {i}");
            }
        }
    }

    #[test]
    fn strokes_use_min_jerk_progress() {
        let mut rng = Rng::seeded(2);
        let from = (0.0, 0.0);
        let to = (400.0, 0.0);
        // A straight stroke shows its timing directly in x.
        let s = stroke(from, to, 300.0, (0.0, 0.0), (8, 40), &mut rng);
        assert!((8..=40).contains(&s.len()));
        let xs: Vec<f64> = s.iter().map(|p| p.x).collect();
        assert!(xs.windows(2).all(|w| w[1] >= w[0] - 1.0), "{xs:?}");
        assert_eq!(*xs.last().unwrap(), 400.0);
        // Slow start: the first fifth of the time covers well under a fifth of the way.
        let mut t = 0.0;
        for p in &s {
            t += p.wait_ms;
            if t >= 60.0 {
                assert!(p.x < 80.0, "{} at {t} ms", p.x);
                break;
            }
        }
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((0.0..=1.0).contains(&min_jerk(t)));
        }
        assert!(min_jerk(0.5) == 0.5 && min_jerk(0.1) < 0.01 && min_jerk(0.9) > 0.99);
    }

    #[test]
    fn far_small_targets_overshoot_and_come_back() {
        let mut rng = Rng::seeded(9);
        let from = (50.0, 400.0);
        let to = (1200.0, 400.0);
        let path = mouse_path(from, to, 30.0, &mut rng);
        let max_x = path.iter().map(|s| s.x).fold(f64::MIN, f64::max);
        assert!(max_x > 1240.0, "goes past: {max_x}");
        let near = mouse_path(from, (400.0, 400.0), 30.0, &mut rng);
        assert!(
            near.iter().all(|s| s.x <= 402.0),
            "no overshoot under 500 px"
        );
    }

    #[test]
    fn press_and_hold_timings_stay_in_range() {
        let mut rng = Rng::seeded(4);
        let holds: Vec<f64> = (0..5_000).map(|_| hold(&mut rng)).collect();
        assert!(holds.iter().all(|h| (45.0..=160.0).contains(h)));
        let mean = holds.iter().sum::<f64>() / holds.len() as f64;
        assert!((80.0..90.0).contains(&mean), "{mean}");
        assert!((0..5_000).all(|_| (40.0..=120.0).contains(&press_pause(&mut rng))));
        assert!((0..5_000).all(|_| (30.0..=90.0).contains(&wheel_gap(&mut rng))));
    }

    #[test]
    fn long_text_is_pasted() {
        assert!(!should_paste(&"a".repeat(60)));
        assert!(should_paste(&"a".repeat(61)));
        // Characters, not bytes.
        assert!(!should_paste(&"é".repeat(60)));
    }

    #[test]
    fn typing_plans_hold_and_space_keys_like_a_person() {
        let mut rng = Rng::seeded(21);
        for _ in 0..300 {
            let text = "Hello all! ab BOB lol";
            let k = keys(text);
            let plan = typing_plan(&k, 0.0, &mut rng);
            assert!(plan.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
            let mut down = vec![None; k.len()];
            let mut shift = false;
            let mut pressed: Vec<&str> = Vec::new();
            for e in &plan {
                match e.step {
                    KeyStep::ShiftDown => {
                        assert!(!shift);
                        shift = true;
                    }
                    KeyStep::ShiftUp => {
                        assert!(shift);
                        shift = false;
                    }
                    KeyStep::Down(i) => {
                        assert_eq!(
                            shift, k[i].shift,
                            "{:?} typed with the wrong Shift",
                            k[i].ch
                        );
                        let code = k[i].code.unwrap();
                        assert!(!pressed.contains(&code), "{code} pressed twice at once");
                        pressed.push(code);
                        down[i] = Some(e.at_ms);
                    }
                    KeyStep::Up(i) => {
                        let code = k[i].code.unwrap();
                        pressed.retain(|c| *c != code);
                        let held = e.at_ms - down[i].unwrap();
                        assert!((49.999..=170.001).contains(&held), "{held}");
                    }
                }
            }
            assert!(!shift, "Shift comes up at the end");
            let downs: Vec<f64> = down.iter().map(|d| d.unwrap()).collect();
            for w in downs.windows(2) {
                let gap = w[1] - w[0];
                assert!((40.0..=900.0).contains(&gap), "{gap}");
            }
        }
        // Rollover: some next key goes down before the last comes up.
        let plan = typing_plan(&keys(&"ab".repeat(40)), 0.0, &mut rng);
        let mut overlap = false;
        let mut open = 0;
        for e in &plan {
            match e.step {
                KeyStep::Down(_) => {
                    open += 1;
                    overlap |= open > 1;
                }
                KeyStep::Up(_) => open -= 1,
                _ => {}
            }
        }
        assert!(overlap);
        // A gap floor applies.
        let plan = typing_plan(&keys("abab"), 300.0, &mut rng);
        let downs: Vec<f64> = plan
            .iter()
            .filter(|e| matches!(e.step, KeyStep::Down(_)))
            .map(|e| e.at_ms)
            .collect();
        assert!(downs.windows(2).all(|w| w[1] - w[0] >= 300.0));
    }

    #[test]
    fn swipes_end_on_target_at_an_even_pace() {
        let mut rng = Rng::seeded(8);
        let path = swipe_path((100.0, 600.0), (100.0, 100.0), 300.0, &mut rng);
        assert!((5..=120).contains(&path.len()));
        let last = path.last().unwrap();
        assert_eq!((last.x, last.y), (100.0, 100.0));
        let total: f64 = path.iter().map(|s| s.wait_ms).sum();
        assert!((270.0..=330.0).contains(&total), "{total}");
        assert!(path.windows(2).all(|w| w[1].y <= w[0].y + 1.0));
    }
}
