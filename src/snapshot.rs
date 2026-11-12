//! Accessibility tree as indented text. Cheap, human-readable, enough for an
//! agent to "see" the page without a screenshot round-trip.
use anyhow::{Context, Result};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxNodeId, AxPropertyName, GetFullAxTreeParams,
};
use serde_json::Value;
use std::collections::HashMap;

/// Longest a snapshot waits for one frame's tree.
const FRAME_TREE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

pub async fn snapshot(page: &Page) -> Result<String> {
    let result = page
        .execute(GetFullAxTreeParams::default())
        .await
        .context("Accessibility.getFullAXTree")?;
    let nodes = &result.result.nodes;
    if nodes.is_empty() {
        return Ok("(empty accessibility tree)".to_string());
    }
    // The tree above holds the top document only; each frame's own tree
    // goes under its Iframe node, from another site or not.
    // Frames with no size (tracking pixels) are left out, and the rest are
    // read at once, each given at most FRAME_TREE_WAIT, so a page of ads
    // can't stall the snapshot.
    let frames = crate::frames::frames(page).await;
    let read = futures::future::join_all(frames.iter().map(|f| async move {
        if !crate::frames::has_size(f).await {
            return None;
        }
        let v = tokio::time::timeout(FRAME_TREE_WAIT, crate::frames::ax_nodes(f))
            .await
            .ok()?
            .ok()?;
        let n = serde_json::from_value::<Vec<AxNode>>(v["nodes"].clone()).ok()?;
        Some((f.key(), n))
    }))
    .await;
    let trees: HashMap<String, Vec<AxNode>> = read.into_iter().flatten().collect();
    let top = page
        .mainframe()
        .await
        .ok()
        .flatten()
        .map(|f| format!("{}:{}", page.target_id().as_ref(), f.as_ref()))
        .unwrap_or_default();
    // Controls with no name (an icon button): what their icon shows.
    let unnamed: Vec<_> = nodes
        .iter()
        .filter(|n| !n.ignored && is_control(n) && !says_anything(name_of(n)))
        .filter_map(|n| n.backend_dom_node_id)
        .take(MAX_ICON_HINTS)
        .collect();
    let icons = crate::server::icon_hints(page, &unnamed).await;
    let ctx = Frames {
        frames: &frames,
        trees: &trees,
        top: &top,
        icons: &icons,
    };
    let mut lines = Vec::new();
    render(nodes, &top, &ctx, 0, &mut lines);
    if lines.is_empty() {
        return Ok("(empty accessibility tree)".to_string());
    }
    Ok(lines.join("\n"))
}

/// The page's frames and their trees, for [`render`].
struct Frames<'a> {
    frames: &'a [crate::frames::Frame],
    trees: &'a HashMap<String, Vec<AxNode>>,
    /// The top document's key, and icon hints for its unnamed controls by
    /// backend node id (ids are only unique within one document's tree).
    top: &'a str,
    icons: &'a HashMap<i64, String>,
}

/// Most unnamed controls a snapshot looks up icon hints for.
const MAX_ICON_HINTS: usize = 60;

/// Roles a person acts on, whose unnamed instances get an icon hint.
fn is_control(node: &AxNode) -> bool {
    matches!(
        node.role
            .as_ref()
            .and_then(|v| v.value.as_ref())
            .and_then(|v| v.as_str()),
        Some("button" | "link" | "menuitem" | "tab" | "switch" | "checkbox" | "radio")
    )
}

/// Whether a name has a letter or digit in it: an icon font's glyph
/// ("✎", or a private-use character) names nothing a person could say.
fn says_anything(name: &str) -> bool {
    name.chars().any(char::is_alphanumeric)
}

/// A node's accessible name, or "".
fn name_of(node: &AxNode) -> &str {
    node.name
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// One document's tree (`key` names its frame), with each frame inside it
/// rendered under its Iframe node.
fn render(nodes: &[AxNode], key: &str, ctx: &Frames, depth: usize, out: &mut Vec<String>) {
    let by_id: HashMap<AxNodeId, &AxNode> = nodes.iter().map(|n| (n.node_id.clone(), n)).collect();
    let has_parent: std::collections::HashSet<AxNodeId> = nodes
        .iter()
        .flat_map(|n| n.child_ids.iter().flatten().cloned())
        .collect();
    let owners: HashMap<i64, &crate::frames::Frame> = ctx
        .frames
        .iter()
        .filter(|f| f.outer_key == key)
        .filter_map(|f| Some((f.owner?, f)))
        .collect();
    for root in nodes.iter().filter(|n| !has_parent.contains(&n.node_id)) {
        walk(root, &by_id, &owners, ctx, key, depth, out);
    }
}

fn walk<'a>(
    node: &'a AxNode,
    by_id: &HashMap<AxNodeId, &'a AxNode>,
    owners: &HashMap<i64, &crate::frames::Frame>,
    ctx: &Frames,
    key: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    if !node.ignored {
        let icon = node
            .backend_dom_node_id
            .filter(|_| key == ctx.top)
            .and_then(|b| ctx.icons.get(b.inner()));
        if let Some(line) = format_node(node, depth, icon.map(String::as_str)) {
            out.push(line);
        }
    }
    let next_depth = if node.ignored { depth } else { depth + 1 };
    if let Some(frame) = node
        .backend_dom_node_id
        .as_ref()
        .and_then(|b| owners.get(b.inner()))
    {
        if let Some(inner) = ctx.trees.get(&frame.key()) {
            render(inner, &frame.key(), ctx, next_depth, out);
        }
    }
    if let Some(child_ids) = node.child_ids.as_ref() {
        for child_id in child_ids {
            if let Some(child) = by_id.get(child_id) {
                walk(child, by_id, owners, ctx, key, next_depth, out);
            }
        }
    }
}

fn format_node(node: &AxNode, depth: usize, icon: Option<&str>) -> Option<String> {
    let role = node
        .role
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let name_str = node
        .name
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .and_then(|v| v.as_str())
        .map(escape_name)
        .unwrap_or_default();
    let value_str = node
        .value
        .as_ref()
        .and_then(|v| v.value.as_ref())
        .map(format_value)
        .unwrap_or_default();
    let level_str = node
        .properties
        .iter()
        .flatten()
        .find(|p| matches!(p.name, AxPropertyName::Level))
        .and_then(|p| p.value.value.as_ref())
        .and_then(|v| v.as_i64())
        .map(|n| format!(" level={n}"))
        .unwrap_or_default();

    // Don't render entirely uninteresting nodes: no role, no name, no value.
    if role == "unknown" && name_str.is_empty() && value_str.is_empty() {
        return None;
    }
    let indent = "  ".repeat(depth);
    let named = if name_str.is_empty() {
        String::new()
    } else {
        format!(" \"{name_str}\"")
    };
    let name_part = match icon.filter(|_| !says_anything(&name_str)) {
        Some(icon) => format!("{named} (icon \"{}\")", escape_name(icon)),
        None => named,
    };
    Some(format!("{indent}{role}{name_part}{value_str}{level_str}"))
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        _ => format!(" [value={}]", serde_json::to_string(v).unwrap_or_default()),
    }
}

fn escape_name(s: &str) -> String {
    // Slice on char boundary so non-ASCII accessibility names (em dashes,
    // smart quotes, CJK, etc.) don't panic mid-codepoint.
    let mut counted = s.char_indices();
    if counted.by_ref().nth(120).is_none() {
        return s.to_string();
    }
    let cut = s.char_indices().nth(117).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", &s[..cut])
}
