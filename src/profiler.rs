//! V8 CPU profiling of a tab's JavaScript via the CDP `Profiler` domain.
//!
//! `start` arms the sampling profiler on the page's CDP session; `stop`
//! collects the profile, writes it as a `.cpuprofile` (opens as a flame graph
//! in Chrome DevTools' Performance panel or https://speedscope.app) and
//! summarizes it: hottest functions, hottest scripts, and the longest
//! stretches the main thread spent busy (the stalls behind an unresponsive
//! page). Profiler state lives on the CDP session, so start/stop need no
//! bookkeeping here as long as the Chrome connection survives in between.
//!
//! `Profiler.enable` does not imply `Runtime.enable`, so this doesn't add the
//! automation tell the forked chromiumoxide avoids.
use anyhow::{Context, Result, bail};
use chromiumoxide::Page;
use chromiumoxide::cdp::js_protocol::profiler::{
    EnableParams, Profile, ProfileNode, SetSamplingIntervalParams, StartParams, StopParams,
    StopReturns,
};
use chromiumoxide::cdp::js_protocol::runtime::TerminateExecutionParams;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default sampling interval. V8's own default is 1000µs; finer catches short
/// functions but makes the profile bigger.
pub const DEFAULT_INTERVAL_US: i64 = 1000;

const LONG_TASK_US: f64 = 50_000.0;

pub async fn start(page: &Page, interval_us: i64) -> Result<()> {
    page.execute(EnableParams::default())
        .await
        .context("Profiler.enable")?;
    page.execute(SetSamplingIntervalParams::new(interval_us))
        .await
        .context("Profiler.setSamplingInterval")?;
    page.execute(StartParams::default())
        .await
        .context("Profiler.start")?;
    Ok(())
}

/// Stop profiling and return the raw profile, plus whether page JS had to be
/// killed to get it.
///
/// The renderer only services a handful of "interrupting" CDP methods while
/// JS is running, and `Profiler.stop` isn't one of them: on a hung page it
/// would wait until the script yields, i.e. forever. So if it hasn't answered
/// after `terminate_after`, send `Runtime.terminateExecution` (which does
/// interrupt) to abort the running task; the stop then lands with the hang
/// fully sampled. `timeout` bounds the whole call.
pub async fn stop(
    page: &Page,
    timeout: Duration,
    terminate_after: Option<Duration>,
) -> Result<(Profile, bool)> {
    let stop = page.execute(StopParams::default());
    tokio::pin!(stop);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut terminated = false;
    if let Some(grace) = terminate_after {
        tokio::select! {
            r = &mut stop => return Ok((stopped(r)?, false)),
            _ = &mut deadline => bail!("Profiler.stop timed out after {}ms", timeout.as_millis()),
            _ = tokio::time::sleep(grace) => {
                page.execute(TerminateExecutionParams::default())
                    .await
                    .context("Runtime.terminateExecution")?;
                terminated = true;
            }
        }
    }
    tokio::select! {
        r = &mut stop => Ok((stopped(r)?, terminated)),
        _ = &mut deadline => bail!("Profiler.stop timed out after {}ms", timeout.as_millis()),
    }
}

fn stopped(
    r: chromiumoxide::error::Result<chromiumoxide::types::CommandResponse<StopReturns>>,
) -> Result<Profile> {
    Ok(
        r.context("Profiler.stop (was start_profiling called on this tab?)")?
            .result
            .profile,
    )
}

pub async fn write(dir: &Path, profile: &Profile) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let stamp = time::OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_default();
    let path = dir.join(format!("{stamp}.cpuprofile"));
    tokio::fs::write(&path, serde_json::to_vec(profile)?).await?;
    Ok(path)
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub duration_ms: f64,
    pub busy_ms: f64,
    pub samples: usize,
    pub top_self: Vec<FnStat>,
    pub top_total: Vec<FnStat>,
    pub top_scripts: Vec<ScriptStat>,
    pub long_tasks: Vec<LongTask>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FnStat {
    pub function: String,
    pub location: String,
    pub ms: f64,
    pub percent: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptStat {
    pub url: String,
    pub self_ms: f64,
    pub percent: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LongTask {
    /// Offset from profile start.
    pub start_ms: f64,
    pub duration_ms: f64,
    /// Browser-native work V8 can't attribute to a script: parsing, style,
    /// layout, paint. A task dominated by this is layout/DOM churn, not one
    /// slow function.
    pub native_ms: f64,
    pub gc_ms: f64,
    /// JS functions with the most self time in this task (`percent` is of the
    /// task).
    pub top_js: Vec<FnStat>,
    /// Scripts with the most self time in this task (`percent` is of the
    /// task). Usually the clearest culprit: a task's self time is often spread
    /// thin across many small functions of one script.
    pub top_scripts: Vec<ScriptStat>,
    /// Stack of the heaviest JS frame, innermost first.
    pub stack: Vec<String>,
}

/// Synthetic V8 nodes that aren't page JS.
fn is_meta(name: &str) -> bool {
    matches!(
        name,
        "(root)" | "(program)" | "(idle)" | "(garbage collector)"
    )
}

fn label(n: &ProfileNode) -> String {
    let f = &n.call_frame;
    if f.function_name.is_empty() {
        "(anonymous)".into()
    } else {
        f.function_name.clone()
    }
}

/// `url:line:col`, 1-based like DevTools shows it.
fn location(n: &ProfileNode) -> String {
    let f = &n.call_frame;
    if f.url.is_empty() {
        return String::new();
    }
    format!("{}:{}:{}", f.url, f.line_number + 1, f.column_number + 1)
}

fn frame(n: &ProfileNode) -> String {
    match location(n) {
        loc if loc.is_empty() => label(n),
        loc => format!("{} {loc}", label(n)),
    }
}

pub fn summarize(p: &Profile, top: usize) -> Summary {
    let by_id: HashMap<i64, &ProfileNode> = p.nodes.iter().map(|n| (n.id, n)).collect();
    let mut parent: HashMap<i64, i64> = HashMap::new();
    for n in &p.nodes {
        for c in n.children.iter().flatten() {
            parent.insert(*c, n.id);
        }
    }
    let samples = p.samples.as_deref().unwrap_or_default();
    let deltas = p.time_deltas.as_deref().unwrap_or_default();

    // Sample i lasts until sample i+1; the last one runs to endTime.
    let mut t = p.start_time;
    let mut times = Vec::with_capacity(samples.len());
    for d in deltas.iter().take(samples.len()) {
        t += *d as f64;
        times.push(t);
    }
    let dur_us = |i: usize| -> f64 {
        let next = times.get(i + 1).copied().unwrap_or(p.end_time);
        (next - times[i]).max(0.0)
    };

    let stack_of = |mut id: i64| -> Vec<i64> {
        let mut s = vec![id];
        while let Some(&up) = parent.get(&id) {
            s.push(up);
            id = up;
        }
        s
    };

    let mut self_us: HashMap<(String, String), f64> = HashMap::new();
    let mut total_us: HashMap<(String, String), f64> = HashMap::new();
    let mut script_us: HashMap<String, f64> = HashMap::new();
    let mut busy_us = 0.0;
    let mut stacks: HashMap<i64, Vec<i64>> = HashMap::new();
    let is_busy = |id: i64| {
        by_id
            .get(&id)
            .is_some_and(|n| n.call_frame.function_name != "(idle)")
    };

    for (i, &id) in samples.iter().enumerate().take(times.len()) {
        let Some(node) = by_id.get(&id) else { continue };
        let d = dur_us(i);
        if node.call_frame.function_name == "(idle)" {
            continue;
        }
        busy_us += d;
        *self_us.entry((label(node), location(node))).or_default() += d;
        if !node.call_frame.url.is_empty() {
            *script_us.entry(node.call_frame.url.clone()).or_default() += d;
        }
        let stack = stacks.entry(id).or_insert_with(|| stack_of(id));
        // Count each function once per sample so recursion doesn't inflate it.
        let mut seen = std::collections::HashSet::new();
        for sid in stack.iter() {
            let n = by_id[sid];
            if is_meta(&n.call_frame.function_name) {
                continue;
            }
            let key = (label(n), location(n));
            if seen.insert(key.clone()) {
                *total_us.entry(key).or_default() += d;
            }
        }
    }

    // Contiguous runs of non-idle samples = main-thread stalls.
    let mut runs: Vec<(usize, usize, f64)> = Vec::new();
    let mut i = 0;
    while i < times.len() {
        if !is_busy(samples[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut d = 0.0;
        while i < times.len() && is_busy(samples[i]) {
            d += dur_us(i);
            i += 1;
        }
        runs.push((start, i, d));
    }
    // Only long tasks (>=50ms, the browser's own threshold) make a page janky.
    runs.retain(|r| r.2 >= LONG_TASK_US);
    runs.sort_by(|a, b| b.2.total_cmp(&a.2));
    let long_tasks = runs
        .iter()
        .take(top.min(5))
        .map(|&(s, e, d)| {
            let (mut native_us, mut gc_us) = (0.0, 0.0);
            let mut js_node_us: HashMap<i64, f64> = HashMap::new();
            let mut js_fn_us: HashMap<(String, String), f64> = HashMap::new();
            let mut script_us: HashMap<String, f64> = HashMap::new();
            for (j, &id) in samples.iter().enumerate().take(e).skip(s) {
                let Some(n) = by_id.get(&id) else { continue };
                let us = dur_us(j);
                match n.call_frame.function_name.as_str() {
                    "(program)" => native_us += us,
                    "(garbage collector)" => gc_us += us,
                    name if is_meta(name) => {}
                    _ => {
                        *js_node_us.entry(id).or_default() += us;
                        *js_fn_us.entry((label(n), location(n))).or_default() += us;
                        if !n.call_frame.url.is_empty() {
                            *script_us.entry(n.call_frame.url.clone()).or_default() += us;
                        }
                    }
                }
            }
            // Heaviest JS leaf, skipping native/GC samples: those carry no
            // stack, so picking them would hide whatever JS the task ran.
            let stack = js_node_us
                .into_iter()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| {
                    stack_of(id)
                        .into_iter()
                        .map(|sid| by_id[&sid])
                        .filter(|n| !is_meta(&n.call_frame.function_name))
                        .take(15)
                        .map(frame)
                        .collect()
                })
                .unwrap_or_default();
            let mut top_js: Vec<_> = js_fn_us.into_iter().collect();
            top_js.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut scripts: Vec<_> = script_us.into_iter().collect();
            scripts.sort_by(|a, b| b.1.total_cmp(&a.1));
            LongTask {
                start_ms: (times[s] - p.start_time) / 1000.0,
                duration_ms: d / 1000.0,
                native_ms: native_us / 1000.0,
                gc_ms: gc_us / 1000.0,
                top_js: top_js
                    .into_iter()
                    .take(3)
                    .map(|((function, location), us)| FnStat {
                        function,
                        location,
                        ms: us / 1000.0,
                        percent: us / d * 100.0,
                    })
                    .collect(),
                top_scripts: scripts
                    .into_iter()
                    .take(3)
                    .map(|(url, us)| ScriptStat {
                        url,
                        self_ms: us / 1000.0,
                        percent: us / d * 100.0,
                    })
                    .collect(),
                stack,
            }
        })
        .collect();

    let pct = |us: f64| {
        if busy_us > 0.0 {
            us / busy_us * 100.0
        } else {
            0.0
        }
    };
    let rank = |m: HashMap<(String, String), f64>| -> Vec<FnStat> {
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.into_iter()
            .take(top)
            .map(|((function, location), us)| FnStat {
                function,
                location,
                ms: us / 1000.0,
                percent: pct(us),
            })
            .collect()
    };
    let mut scripts: Vec<_> = script_us.into_iter().collect();
    scripts.sort_by(|a, b| b.1.total_cmp(&a.1));

    Summary {
        duration_ms: (p.end_time - p.start_time) / 1000.0,
        busy_ms: busy_us / 1000.0,
        samples: samples.len(),
        top_self: rank(self_us),
        top_total: rank(total_us),
        top_scripts: scripts
            .into_iter()
            .take(top)
            .map(|(url, us)| ScriptStat {
                url,
                self_ms: us / 1000.0,
                percent: pct(us),
            })
            .collect(),
        long_tasks,
    }
}

pub fn render(s: &Summary, path: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Profiled {:.0}ms, main thread busy {:.0}ms ({} samples).\nSaved {} (open in DevTools Performance panel or speedscope.app for a flame graph).",
        s.duration_ms,
        s.busy_ms,
        s.samples,
        path.display()
    );
    let _ = writeln!(out, "\nLong tasks >=50ms:");
    if s.long_tasks.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for t in &s.long_tasks {
        let js_ms = t.duration_ms - t.native_ms - t.gc_ms;
        let _ = writeln!(
            out,
            "  {:.0}ms at +{:.0}ms: JS {:.0}ms, browser-native {:.0}ms (parse/style/layout), GC {:.0}ms",
            t.duration_ms,
            t.start_ms,
            js_ms.max(0.0),
            t.native_ms,
            t.gc_ms
        );
        for sc in &t.top_scripts {
            let _ = writeln!(
                out,
                "    {:>7.1}ms {:>5.1}%  script {}",
                sc.self_ms,
                sc.percent,
                short_url(&sc.url)
            );
        }
        for f in &t.top_js {
            let _ = writeln!(
                out,
                "    {:>7.1}ms {:>5.1}%  {} {}",
                f.ms,
                f.percent,
                f.function,
                short_loc(&f.location)
            );
        }
        if !t.stack.is_empty() {
            let _ = writeln!(out, "    heaviest JS stack, innermost first:");
            for f in &t.stack {
                let _ = writeln!(out, "      {}", short_frame(f));
            }
        }
    }
    let _ = writeln!(out, "\nTop self time:");
    for f in &s.top_self {
        let _ = writeln!(
            out,
            "  {:>8.1}ms {:>5.1}%  {} {}",
            f.ms,
            f.percent,
            f.function,
            short_loc(&f.location)
        );
    }
    let _ = writeln!(out, "\nTop total time:");
    for f in &s.top_total {
        let _ = writeln!(
            out,
            "  {:>8.1}ms {:>5.1}%  {} {}",
            f.ms,
            f.percent,
            f.function,
            short_loc(&f.location)
        );
    }
    let _ = writeln!(out, "\nSelf time by script:");
    for sc in &s.top_scripts {
        let _ = writeln!(
            out,
            "  {:>8.1}ms {:>5.1}%  {}",
            sc.self_ms,
            sc.percent,
            short_url(&sc.url)
        );
    }
    out
}

/// Text-output form of a script URL. Full URLs stay in the structured result
/// and the .cpuprofile; here a `data:` page or a long query string would bury
/// the summary, so keep the start (scheme, host) and the end (file name).
fn short_url(url: &str) -> String {
    const MAX: usize = 100;
    if let Some(rest) = url.strip_prefix("data:") {
        let kind: String = rest.chars().take_while(|&c| c != ',').take(30).collect();
        return format!("data:{kind},…");
    }
    let chars: Vec<char> = url.chars().collect();
    if chars.len() <= MAX {
        return url.to_string();
    }
    let head: String = chars[..40].iter().collect();
    let tail: String = chars[chars.len() - 55..].iter().collect();
    format!("{head}…{tail}")
}

/// `short_url` for a `url:line:col` location.
fn short_loc(loc: &str) -> String {
    let mut parts = loc.rsplitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(col), Some(line), Some(url))
            if col.chars().all(|c| c.is_ascii_digit())
                && line.chars().all(|c| c.is_ascii_digit()) =>
        {
            format!("{}:{line}:{col}", short_url(url))
        }
        _ => short_url(loc),
    }
}

/// `short_loc` for a rendered `name url:line:col` frame.
fn short_frame(frame: &str) -> String {
    match frame.split_once(' ') {
        Some((name, loc)) => format!("{name} {}", short_loc(loc)),
        None => frame.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{short_loc, short_url};

    #[test]
    fn shortens_urls_for_display() {
        assert_eq!(short_url("https://a.com/x.js"), "https://a.com/x.js");
        assert_eq!(
            short_url("data:text/html,<script>long</script>"),
            "data:text/html,…"
        );
        let long = format!("https://example.com/{}/app.js?cb=1", "a".repeat(200));
        let s = short_url(&long);
        assert!(s.starts_with("https://example.com/") && s.ends_with("/app.js?cb=1"));
        assert!(s.chars().count() <= 100);
        assert_eq!(
            short_loc("data:text/html,%3Cp%3E:2:19"),
            "data:text/html,…:2:19"
        );
        assert_eq!(
            short_loc("https://a.com/x.js:10:5"),
            "https://a.com/x.js:10:5"
        );
    }
}
