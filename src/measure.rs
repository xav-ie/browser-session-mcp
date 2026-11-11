//! `measure_page`: how fast a page loads, from the browser's own timing
//! records, in the session as it is (its login, phone settings and
//! throttling): server response, first and largest paint, layout shift,
//! blocking time from long tasks, and bytes by type and host. A small,
//! dependency-free stand-in for a Lighthouse performance run.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Read after the page has loaded and its requests have gone quiet. Long
/// tasks, layout shifts and paints are buffered by the browser from the
/// start of the load, so observing them afterwards still sees them all.
pub const COLLECT_JS: &str = r#"function() {
  return new Promise(function(done) {
    var got = { lcp: [], cls: [], long: [], paint: [] };
    var watch = function(type, into) {
      try {
        new PerformanceObserver(function(l) { Array.prototype.push.apply(got[into], l.getEntries()); })
          .observe({ type: type, buffered: true });
      } catch (e) {}
    };
    watch('largest-contentful-paint', 'lcp');
    watch('layout-shift', 'cls');
    watch('longtask', 'long');
    watch('paint', 'paint');
    setTimeout(function() {
      var describe = function(el) {
        if (!el || !el.tagName) return null;
        var t = el.tagName.toLowerCase();
        if (el.id) t += '#' + el.id;
        var text = String(el.innerText || el.getAttribute('alt') || el.currentSrc || '').replace(/\s+/g, ' ').trim();
        if (text) t += ' "' + (text.length > 60 ? text.slice(0, 59) + '...' : text) + '"';
        return t;
      };
      var nav = performance.getEntriesByType('navigation')[0] || {};
      var fcpE = got.paint.filter(function(e) { return e.name === 'first-contentful-paint'; })[0];
      var fcp = fcpE ? fcpE.startTime : null;
      var lcpE = got.lcp[got.lcp.length - 1];
      // Layout shift as Core Web Vitals counts it: the worst window of
      // shifts at most 1 s apart and 5 s long, ignoring ones after input.
      var cls = 0, win = 0, first = 0, last = 0;
      got.cls.forEach(function(e) {
        if (e.hadRecentInput) return;
        if (win && (e.startTime - last > 1000 || e.startTime - first > 5000)) { cls = Math.max(cls, win); win = 0; }
        if (!win) first = e.startTime;
        win += e.value; last = e.startTime;
      });
      cls = Math.max(cls, win);
      // Blocking time as Lighthouse counts it: each long task's time past
      // 50 ms, after the first paint.
      var tbt = 0, longest = 0;
      got.long.forEach(function(e) {
        longest = Math.max(longest, e.duration);
        if (fcp === null || e.startTime >= fcp) tbt += Math.max(0, e.duration - 50);
      });
      var kind = function(r) {
        var u = r.name.split('?')[0].toLowerCase();
        if (r.initiatorType === 'script' || /\.m?js$/.test(u)) return 'script';
        if (r.initiatorType === 'css' || /\.css$/.test(u)) return 'stylesheet';
        if (r.initiatorType === 'img' || r.initiatorType === 'image' || /\.(png|jpe?g|gif|webp|avif|svg|ico)$/.test(u)) return 'image';
        if (/\.(woff2?|ttf|otf|eot)$/.test(u)) return 'font';
        if (r.initiatorType === 'fetch' || r.initiatorType === 'xmlhttprequest') return 'fetch';
        if (r.initiatorType === 'video' || r.initiatorType === 'audio') return 'media';
        return 'other';
      };
      var byType = {}, byHost = {}, largest = [], bytes = nav.transferSize || 0;
      performance.getEntriesByType('resource').forEach(function(r) {
        var size = r.transferSize || r.encodedBodySize || 0;
        var host = ''; try { host = new URL(r.name).host; } catch (e) {}
        var k = kind(r);
        byType[k] = byType[k] || { requests: 0, bytes: 0 };
        byType[k].requests++; byType[k].bytes += size;
        byHost[host] = byHost[host] || { requests: 0, bytes: 0 };
        byHost[host].requests++; byHost[host].bytes += size;
        bytes += size;
        largest.push({ url: r.name.slice(0, 200), type: k, bytes: size, ms: Math.round(r.duration) });
      });
      largest.sort(function(a, b) { return b.bytes - a.bytes; });
      var hosts = Object.keys(byHost).map(function(h) { return { host: h, requests: byHost[h].requests, bytes: byHost[h].bytes }; })
        .sort(function(a, b) { return b.bytes - a.bytes; }).slice(0, 8);
      var types = Object.keys(byType).map(function(t) { return { type: t, requests: byType[t].requests, bytes: byType[t].bytes }; })
        .sort(function(a, b) { return b.bytes - a.bytes; });
      var ms = function(v) { return v === null || v === undefined || isNaN(v) ? null : Math.round(v); };
      done({
        ttfbMs: ms(nav.responseStart), fcpMs: ms(fcp), lcpMs: ms(lcpE && lcpE.startTime),
        lcpElement: describe(lcpE && lcpE.element), cls: Math.round(cls * 1000) / 1000,
        tbtMs: ms(tbt), longTasks: got.long.length, longestTaskMs: ms(longest),
        domContentLoadedMs: ms(nav.domContentLoadedEventEnd), loadMs: ms(nav.loadEventEnd),
        requests: performance.getEntriesByType('resource').length + 1, bytes: bytes,
        byType: types, byHost: hosts, largest: largest.slice(0, 5)
      });
    }, 500);
  });
}"#;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Share {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct Resource {
    pub url: String,
    pub r#type: String,
    pub bytes: u64,
    pub ms: u64,
}

/// One load of a page, measured.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Measure {
    pub url: String,
    /// The session's device and throttling during the load.
    pub conditions: String,
    /// Time to the first byte of the page from the server.
    pub ttfb_ms: Option<u64>,
    /// First contentful paint.
    pub fcp_ms: Option<u64>,
    /// Largest contentful paint, and the element it was.
    pub lcp_ms: Option<u64>,
    pub lcp_element: Option<String>,
    /// Cumulative layout shift.
    pub cls: f64,
    /// Total blocking time: long tasks' time past 50 ms, after first paint.
    pub tbt_ms: Option<u64>,
    pub long_tasks: u64,
    pub longest_task_ms: Option<u64>,
    pub dom_content_loaded_ms: Option<u64>,
    pub load_ms: Option<u64>,
    pub requests: u64,
    /// Bytes transferred (cross-site resources without timing headers count 0).
    pub bytes: u64,
    pub by_type: Vec<Share>,
    pub by_host: Vec<Share>,
    pub largest: Vec<Resource>,
    /// Each metric as good, needs improvement or poor, by the Core Web Vitals
    /// and Lighthouse thresholds.
    #[serde(default)]
    pub ratings: std::collections::BTreeMap<String, String>,
}

/// (metric, good at or under, poor over)
const THRESHOLDS: &[(&str, f64, f64)] = &[
    ("ttfb", 800.0, 1800.0),
    ("fcp", 1800.0, 3000.0),
    ("lcp", 2500.0, 4000.0),
    ("cls", 0.1, 0.25),
    ("tbt", 200.0, 600.0),
];

fn rate(value: f64, good: f64, poor: f64) -> &'static str {
    if value <= good {
        "good"
    } else if value <= poor {
        "needs improvement"
    } else {
        "poor"
    }
}

impl Measure {
    pub fn rate(&mut self) {
        for (name, good, poor) in THRESHOLDS {
            let v = match *name {
                "ttfb" => self.ttfb_ms.map(|v| v as f64),
                "fcp" => self.fcp_ms.map(|v| v as f64),
                "lcp" => self.lcp_ms.map(|v| v as f64),
                "cls" => Some(self.cls),
                _ => self.tbt_ms.map(|v| v as f64),
            };
            if let Some(v) = v {
                self.ratings
                    .insert((*name).to_string(), rate(v, *good, *poor).to_string());
            }
        }
    }

    pub fn render(&self) -> String {
        let ms = |v: Option<u64>| v.map_or("n/a".to_string(), |v| format!("{v} ms"));
        let r = |k: &str| {
            self.ratings
                .get(k)
                .map_or(String::new(), |r| format!(" ({r})"))
        };
        let kb = |b: u64| format!("{:.0} KB", b as f64 / 1024.0);
        let mut s = format!(
            "Measured {} ({}).\nServer response {}{}, first paint {}{}, largest paint {}{}{}, layout shift {}{}, blocking time {}{} over {} long tasks (longest {}).\nDOMContentLoaded {}, load {}; {} requests, {}.",
            self.url,
            self.conditions,
            ms(self.ttfb_ms),
            r("ttfb"),
            ms(self.fcp_ms),
            r("fcp"),
            ms(self.lcp_ms),
            r("lcp"),
            self.lcp_element
                .as_ref()
                .map_or(String::new(), |e| format!(" on {e}")),
            self.cls,
            r("cls"),
            ms(self.tbt_ms),
            r("tbt"),
            self.long_tasks,
            ms(self.longest_task_ms),
            ms(self.dom_content_loaded_ms),
            ms(self.load_ms),
            self.requests,
            kb(self.bytes),
        );
        let types: Vec<String> = self
            .by_type
            .iter()
            .map(|t| {
                format!(
                    "{} {} ({})",
                    t.r#type.as_deref().unwrap_or("?"),
                    kb(t.bytes),
                    t.requests
                )
            })
            .collect();
        s.push_str(&format!("\nBy type: {}.", types.join(", ")));
        let hosts: Vec<String> = self
            .by_host
            .iter()
            .map(|h| {
                format!(
                    "{} {} ({})",
                    h.host.as_deref().unwrap_or("?"),
                    kb(h.bytes),
                    h.requests
                )
            })
            .collect();
        s.push_str(&format!("\nBy host: {}.", hosts.join(", ")));
        for l in &self.largest {
            s.push_str(&format!(
                "\n  {} {} {} ms {}",
                kb(l.bytes),
                l.r#type,
                l.ms,
                l.url
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_follow_the_thresholds() {
        assert_eq!(rate(2500.0, 2500.0, 4000.0), "good");
        assert_eq!(rate(3000.0, 2500.0, 4000.0), "needs improvement");
        assert_eq!(rate(0.3, 0.1, 0.25), "poor");
    }
}
