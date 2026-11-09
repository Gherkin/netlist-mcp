//! MCP Apps UI (issue #32): the `ui://` HTML resource that draws a walk
//! topology as a schematic, and the tool metadata that links `draw_walk` to
//! it. Hosts without MCP Apps support ignore both and show the tool's JSON
//! text, so nothing here changes what a text-only client sees.
//!
//! Spec: modelcontextprotocol/ext-apps, specification/2026-01-26/apps.mdx.

use rmcp::model::{ClientCapabilities, Meta, ReadResourceResult, Resource, ResourceContents, Tool};
use serde_json::json;

/// URI of the walk-topology drawing. Must use the `ui://` scheme.
pub const WALK_TOPOLOGY_URI: &str = "ui://netlist/walk-topology.html";

/// The capability key a client sets when it can show MCP App views.
pub const UI_EXTENSION: &str = "io.modelcontextprotocol/ui";

/// Does this client show MCP App views? The spec puts the key under
/// `extensions`; `experimental` is accepted too, for hosts that used it
/// before extensions existed.
pub fn client_supports_ui(caps: &ClientCapabilities) -> bool {
    caps.extensions.as_ref().is_some_and(|m| m.contains_key(UI_EXTENSION))
        || caps.experimental.as_ref().is_some_and(|m| m.contains_key(UI_EXTENSION))
}

/// A UI tool is one whose `_meta` links a view (`ui.resourceUri`).
fn is_ui_tool(t: &Tool) -> bool {
    t.meta.as_ref().is_some_and(|m| m.get("ui").and_then(|u| u.get("resourceUri")).is_some())
}

/// The tool list a client sees: UI tools are left out unless it shows views
/// (the spec says servers SHOULD gate them). They stay callable either way.
pub fn visible_tools(tools: Vec<Tool>, supports_ui: bool) -> Vec<Tool> {
    tools.into_iter().filter(|t| supports_ui || !is_ui_tool(t)).collect()
}

/// The only mime type the spec allows for an MCP App resource.
pub const MCP_APP_MIME: &str = "text/html;profile=mcp-app";

/// The page itself. Self-contained: no external scripts, styles or fonts, so
/// the host's default (restrictive) CSP is enough and no `csp` is declared.
const WALK_TOPOLOGY_HTML: &str = include_str!("ui/walk_topology.html");

fn meta(value: serde_json::Value) -> Meta {
    match value {
        serde_json::Value::Object(map) => Meta(map),
        _ => unreachable!("meta is always built from a JSON object"),
    }
}

/// `_meta` for the `draw_walk` tool. Sets both the current nested key and the
/// deprecated flat `ui/resourceUri`, as the reference SDK does, so hosts on
/// either form find the resource.
pub fn draw_walk_tool_meta() -> Meta {
    meta(json!({
        "ui": { "resourceUri": WALK_TOPOLOGY_URI },
        "ui/resourceUri": WALK_TOPOLOGY_URI,
    }))
}

/// `_meta.ui` on the resource contents: ask the host for a border, since the
/// page has no frame of its own.
fn resource_meta() -> Meta {
    meta(json!({ "ui": { "prefersBorder": true } }))
}

/// Every UI resource this server offers, for `resources/list`.
pub fn resources() -> Vec<Resource> {
    vec![Resource::new(WALK_TOPOLOGY_URI, "walk_topology")
        .with_title("Walk topology schematic")
        .with_description(
            "Draws a draw_walk result as a schematic: R, C, L, FB and D as \
             symbols, ICs and connectors as boxes, rails as ground/supply \
             symbols, DNP parts dashed.",
        )
        .with_mime_type(MCP_APP_MIME)
        .with_size(WALK_TOPOLOGY_HTML.len() as u64)]
}

/// `resources/read` for `uri`, or None when it is not one of ours.
pub fn read(uri: &str) -> Option<ReadResourceResult> {
    (uri == WALK_TOPOLOGY_URI).then(|| {
        ReadResourceResult::new(vec![ResourceContents::text(WALK_TOPOLOGY_HTML, uri)
            .with_mime_type(MCP_APP_MIME)
            .with_meta(resource_meta())])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_meta_links_the_resource_under_both_keys() {
        let m = serde_json::to_value(draw_walk_tool_meta()).unwrap();
        assert_eq!(m["ui"]["resourceUri"], WALK_TOPOLOGY_URI);
        assert_eq!(m["ui/resourceUri"], WALK_TOPOLOGY_URI);
    }

    #[test]
    fn resource_is_listed_with_the_app_mime_type() {
        let list = resources();
        assert_eq!(list.len(), 1);
        let r = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(r["uri"], WALK_TOPOLOGY_URI);
        assert_eq!(r["mimeType"], MCP_APP_MIME);
        assert!(WALK_TOPOLOGY_URI.starts_with("ui://"));
    }

    #[test]
    fn read_returns_the_html_with_mime_and_meta() {
        let res = serde_json::to_value(read(WALK_TOPOLOGY_URI).expect("ours")).unwrap();
        let c = &res["contents"][0];
        assert_eq!(c["uri"], WALK_TOPOLOGY_URI);
        assert_eq!(c["mimeType"], MCP_APP_MIME);
        assert_eq!(c["_meta"]["ui"]["prefersBorder"], true);
        let text = c["text"].as_str().unwrap();
        assert!(text.trim_start().to_ascii_lowercase().starts_with("<!doctype html>"));
        // The page must speak the MCP Apps handshake and take the result.
        for needle in [
            "ui/initialize",
            "ui/notifications/initialized",
            "ui/notifications/tool-result",
            "ui/notifications/size-changed",
        ] {
            assert!(text.contains(needle), "page lacks {needle}");
        }
    }

    #[test]
    fn page_loads_nothing_from_outside() {
        // No declared CSP, so any external fetch would be blocked by the host.
        let lower = WALK_TOPOLOGY_HTML.to_ascii_lowercase();
        for needle in ["src=\"http", "href=\"http", "@import", "url(http"] {
            assert!(!lower.contains(needle), "page references external {needle}");
        }
    }

    fn caps(v: serde_json::Value) -> ClientCapabilities {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn ui_support_is_read_from_extensions_or_experimental() {
        assert!(client_supports_ui(&caps(json!({
            "extensions": { UI_EXTENSION: { "mimeTypes": [MCP_APP_MIME] } }
        }))));
        assert!(client_supports_ui(&caps(json!({ "experimental": { UI_EXTENSION: {} } }))));
        assert!(!client_supports_ui(&caps(json!({}))));
        assert!(!client_supports_ui(&caps(json!({ "extensions": { "other/ext": {} } }))));
    }

    #[test]
    fn draw_walk_is_listed_only_to_ui_clients() {
        let schema = std::sync::Arc::new(serde_json::Map::new());
        let tools = || {
            let mut draw = Tool::new("draw_walk", "d", schema.clone());
            draw.meta = Some(draw_walk_tool_meta());
            vec![Tool::new("walk", "w", schema.clone()), draw]
        };
        let names = |ts: Vec<Tool>| ts.into_iter().map(|t| t.name.to_string()).collect::<Vec<_>>();
        assert_eq!(names(visible_tools(tools(), true)), ["walk", "draw_walk"]);
        assert_eq!(names(visible_tools(tools(), false)), ["walk"]);
    }

    #[test]
    fn read_rejects_other_uris() {
        assert!(read("ui://netlist/other.html").is_none());
        assert!(read("file:///etc/passwd").is_none());
    }
}
