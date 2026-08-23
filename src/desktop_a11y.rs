//! Semantic Linux desktop navigation over the AT-SPI accessibility bus.
//!
//! Accessible nodes are returned with opaque locators.  Agents can activate,
//! focus, or edit those nodes without guessing pixel coordinates.  The visual
//! desktop tool remains available for canvases and inaccessible applications.

use std::cmp::Reverse;
use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::zbus::proxy::CacheProperties;
use atspi::{AccessibilityConnection, CoordType, Interface, State};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const CALL_TIMEOUT: Duration = Duration::from_millis(900);
const MAX_VISITED: usize = 3_000;
const MAX_DEPTH: u16 = 24;
const MAX_LIMIT: usize = 400;
const MAX_QUERY_CHARS: usize = 300;
const MAX_TARGET_CHARS: usize = 4_096;
const MAX_TEXT_CHARS: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Locator {
    bus: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Serialize)]
struct SemanticNode {
    target: String,
    role: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bounds: Option<Bounds>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<String>,
    editable: bool,
    focused: bool,
    active: bool,
    depth: u16,
    #[serde(skip)]
    score: i32,
}

pub async fn inspect(query: &str, requested_limit: usize) -> Result<Value> {
    validate_chars("query", query, MAX_QUERY_CHARS)?;
    let limit = requested_limit.clamp(1, MAX_LIMIT);
    let needle = query.trim().to_lowercase();
    let connection = connect().await?;
    let root = timeout(CALL_TIMEOUT, connection.root_accessible_on_registry())
        .await
        .context("AT-SPI registry did not respond")??;
    let roots = timeout(CALL_TIMEOUT, root.get_children())
        .await
        .context("AT-SPI application list did not respond")??;

    let mut queue = VecDeque::new();
    queue.extend(roots.into_iter().map(|object| (object, 0u16)));
    let mut nodes = Vec::new();
    let mut visited = 0usize;
    let mut failed = 0usize;

    let visit_budget = if needle.is_empty() {
        limit.saturating_mul(6).clamp(300, 1_800)
    } else {
        MAX_VISITED
    };
    while let Some((object, depth)) = queue.pop_front() {
        if visited >= visit_budget {
            break;
        }
        visited += 1;
        let proxy = match timeout(
            CALL_TIMEOUT,
            object.as_accessible_proxy(connection.connection()),
        )
        .await
        {
            Ok(Ok(proxy)) => proxy,
            _ => {
                failed += 1;
                continue;
            }
        };

        let (name, description, role, states, interfaces, children) = tokio::join!(
            proxy.name(),
            proxy.description(),
            proxy.get_role(),
            proxy.get_state(),
            proxy.get_interfaces(),
            proxy.get_children(),
        );
        let name = compact(&name.unwrap_or_default(), 240);
        let description = compact(&description.unwrap_or_default(), 320);
        let role = role
            .map(|value| value.name().to_string())
            .unwrap_or_default();
        let states = states.unwrap_or_default();
        let interfaces = interfaces.unwrap_or_default();
        let focused = states.contains(State::Focused);
        let active = states.contains(State::Active);
        let showing = states.contains(State::Showing) || states.contains(State::Visible);
        let editable = interfaces.contains(Interface::EditableText);
        let actionable = interfaces.contains(Interface::Action);
        let component = interfaces.contains(Interface::Component);

        let mut actions = Vec::new();
        let mut bounds = None;
        if actionable || component {
            if let Ok(proxies) = proxy.proxies().await {
                if actionable
                    && let Ok(action_proxy) = proxies.action().await
                    && let Ok(Ok(remote_actions)) =
                        timeout(CALL_TIMEOUT, action_proxy.get_actions()).await
                {
                    actions = remote_actions
                        .into_iter()
                        .map(|action| compact(&action.name, 80))
                        .filter(|action| !action.is_empty())
                        .take(12)
                        .collect();
                }
                if component
                    && let Ok(component_proxy) = proxies.component().await
                    && let Ok(Ok((x, y, width, height))) =
                        timeout(CALL_TIMEOUT, component_proxy.get_extents(CoordType::Screen)).await
                    && width > 0
                    && height > 0
                {
                    bounds = Some(Bounds {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
        }

        let haystack = format!(
            "{} {} {} {}",
            name.to_lowercase(),
            description.to_lowercase(),
            role.to_lowercase(),
            actions.join(" ").to_lowercase()
        );
        let matches_query = needle.is_empty() || haystack.contains(&needle);
        let useful =
            depth <= 1 || focused || active || editable || actionable || is_semantic_role(&role);
        if matches_query && useful && (showing || depth <= 1 || focused || active) {
            let target = encode_target(&Locator {
                bus: object.name().map(ToString::to_string).unwrap_or_default(),
                path: object.path().to_string(),
            })?;
            let score = semantic_score(
                &needle, &haystack, depth, showing, focused, active, editable, actionable,
            );
            nodes.push(SemanticNode {
                target,
                role,
                name,
                description: (!description.is_empty()).then_some(description),
                bounds,
                actions,
                editable,
                focused,
                active,
                depth,
                score,
            });
        }

        // Application roots do not consistently expose Showing, but their
        // top-level windows do.  Pruning hidden window subtrees keeps a normal
        // inspect comfortably below a second even when browsers expose many
        // background tabs.
        if depth < MAX_DEPTH
            && (depth == 0 || showing || focused || active)
            && let Ok(children) = children
        {
            queue.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }

    nodes.sort_by_key(|node| Reverse(node.score));
    let matched = nodes.len();
    nodes.truncate(limit);
    Ok(json!({
        "backend": "at-spi",
        "query": query,
        "nodes": nodes,
        "returned": nodes.len(),
        "matched": matched,
        "visited": visited,
        "failed_nodes": failed,
        "truncated": matched > limit || visited >= visit_budget,
        "hint": "Prefer target-based activate/set_text/focus. Use observe and coordinates only when the required control is absent from this semantic tree."
    }))
}

pub async fn activate(target: &str, requested_action: Option<&str>) -> Result<Value> {
    validate_chars("target", target, MAX_TARGET_CHARS)?;
    if let Some(action) = requested_action {
        validate_chars("action_name", action, 100)?;
    }
    let locator = decode_target(target)?;
    let connection = connect().await?;
    let proxy = target_proxy(&connection, &locator).await?;
    let proxies = proxy.proxies().await?;
    let action_proxy = proxies
        .action()
        .await
        .context("target does not expose an accessible action")?;
    let actions = timeout(CALL_TIMEOUT, action_proxy.get_actions())
        .await
        .context("accessible action list timed out")??;
    if actions.is_empty() {
        bail!("target exposes no accessible actions");
    }
    let index = if let Some(requested) = requested_action.map(str::trim).filter(|v| !v.is_empty()) {
        actions
            .iter()
            .position(|action| action.name.eq_ignore_ascii_case(requested))
            .with_context(|| {
                format!(
                    "target has no `{requested}` action; available actions: {}",
                    actions
                        .iter()
                        .map(|action| action.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?
    } else {
        0
    };
    let ok = timeout(CALL_TIMEOUT, action_proxy.do_action(index as i32))
        .await
        .context("accessible action timed out")??;
    if !ok {
        bail!("application refused the accessible action");
    }
    Ok(json!({"backend": "at-spi", "ok": true, "action": actions[index].name}))
}

pub async fn set_text(target: &str, text: &str) -> Result<Value> {
    validate_chars("target", target, MAX_TARGET_CHARS)?;
    validate_chars("text", text, MAX_TEXT_CHARS)?;
    let locator = decode_target(target)?;
    let connection = connect().await?;
    let proxy = target_proxy(&connection, &locator).await?;
    let proxies = proxy.proxies().await?;
    let editable = proxies
        .editable_text()
        .await
        .context("target is not an editable text control")?;
    let ok = timeout(CALL_TIMEOUT, editable.set_text_contents(text))
        .await
        .context("setting accessible text timed out")??;
    if !ok {
        bail!("application refused to replace the control text");
    }
    Ok(json!({
        "backend": "at-spi",
        "ok": true,
        "action": "set_text",
        "characters": text.chars().count()
    }))
}

pub async fn focus(target: &str) -> Result<Value> {
    validate_chars("target", target, MAX_TARGET_CHARS)?;
    let locator = decode_target(target)?;
    let connection = connect().await?;
    let proxy = target_proxy(&connection, &locator).await?;
    let proxies = proxy.proxies().await?;
    let component = proxies
        .component()
        .await
        .context("target cannot receive accessible focus")?;
    let ok = timeout(CALL_TIMEOUT, component.grab_focus())
        .await
        .context("accessible focus timed out")??;
    if !ok {
        bail!("application refused to focus the control");
    }
    Ok(json!({"backend": "at-spi", "ok": true, "action": "focus"}))
}

async fn connect() -> Result<AccessibilityConnection> {
    timeout(CONNECT_TIMEOUT, AccessibilityConnection::new())
        .await
        .context("AT-SPI connection timed out")?
        .context(
            "AT-SPI is unavailable. Ensure the desktop accessibility bus is running; visual navigation remains available as fallback",
        )
}

async fn target_proxy<'a>(
    connection: &'a AccessibilityConnection,
    locator: &'a Locator,
) -> Result<AccessibleProxy<'a>> {
    if locator.bus.is_empty() || !locator.bus.starts_with(':') {
        bail!("invalid AT-SPI target bus name");
    }
    if !locator.path.starts_with("/org/a11y/atspi/") {
        bail!("invalid AT-SPI target object path");
    }
    timeout(
        CALL_TIMEOUT,
        AccessibleProxy::builder(connection.connection())
            .destination(locator.bus.as_str())?
            .path(locator.path.as_str())?
            .cache_properties(CacheProperties::No)
            .build(),
    )
    .await
    .context("AT-SPI target did not respond")?
    .context("AT-SPI target no longer exists; inspect the interface again")
}

fn encode_target(locator: &Locator) -> Result<String> {
    let bytes = serde_json::to_vec(locator)?;
    Ok(format!(
        "a11y:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn decode_target(target: &str) -> Result<Locator> {
    let encoded = target
        .trim()
        .strip_prefix("a11y:")
        .context("semantic target must start with `a11y:`")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .context("semantic target is not valid base64url")?;
    if bytes.len() > 2_048 {
        bail!("semantic target is too large");
    }
    serde_json::from_slice(&bytes).context("semantic target payload is invalid")
}

fn semantic_score(
    needle: &str,
    haystack: &str,
    depth: u16,
    showing: bool,
    focused: bool,
    active: bool,
    editable: bool,
    actionable: bool,
) -> i32 {
    let mut score = -(depth as i32);
    score += i32::from(showing) * 30;
    score += i32::from(actionable) * 30;
    score += i32::from(editable) * 45;
    score += i32::from(active) * 120;
    score += i32::from(focused) * 180;
    if !needle.is_empty() && haystack.contains(needle) {
        score += 300;
    }
    score
}

fn is_semantic_role(role: &str) -> bool {
    matches!(
        role,
        "application"
            | "frame"
            | "dialog"
            | "alert"
            | "button"
            | "push button"
            | "check box"
            | "combo box"
            | "entry"
            | "password text"
            | "menu item"
            | "page tab"
            | "radio button"
            | "slider"
            | "spin button"
            | "table cell"
            | "text"
            | "toggle button"
            | "tree item"
    )
}

fn compact(value: &str, max_chars: usize) -> String {
    let mut output = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if output.chars().count() > max_chars {
        output = output.chars().take(max_chars).collect();
        output.push('…');
    }
    output
}

fn validate_chars(name: &str, value: &str, maximum: usize) -> Result<()> {
    if value.chars().count() > maximum {
        bail!("{name} is too long (maximum {maximum} characters)");
    }
    if value.contains('\0') {
        bail!("{name} contains a NUL byte");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_targets_round_trip_without_shell_syntax() {
        let locator = Locator {
            bus: ":1.42".into(),
            path: "/org/a11y/atspi/accessible/815".into(),
        };
        let target = encode_target(&locator).unwrap();
        assert!(target.starts_with("a11y:"));
        assert!(!target.contains('/'));
        assert_eq!(decode_target(&target).unwrap(), locator);
    }

    #[test]
    fn malformed_or_oversized_targets_are_rejected() {
        assert!(decode_target("not-a-target").is_err());
        assert!(decode_target("a11y:***").is_err());
        assert!(validate_chars("text", &"x".repeat(MAX_TEXT_CHARS + 1), MAX_TEXT_CHARS).is_err());
    }

    #[test]
    fn compact_preserves_unicode_boundaries() {
        assert_eq!(compact("  Întreabă   GnomeAI  ", 20), "Întreabă GnomeAI");
        assert_eq!(compact("ăăăă", 3), "ăăă…");
    }
}
