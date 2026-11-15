//! Device MCP protocol seam; tool definitions are supplied by the frozen live baseline.
use crate::model::Result;
use serde_json::{json, Value};

pub fn respond(module: &str, message: &Value) -> Result<Option<Value>> {
    let result = match message["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":format!("maxa-device-{module}"),"version":"0.1.0"}})
        }
        Some("notifications/initialized") => return Ok(Some(json!({}))),
        Some("tools/list") => json!({"tools":catalog(module)?}),
        Some("tools/call") => return Ok(None),
        _ => return Err("MCP method is not public".into()),
    };
    Ok(Some(
        json!({"jsonrpc":"2.0","id":message["id"],"result":result}),
    ))
}

fn catalog(module: &str) -> Result<Value> {
    let source = match module {
        "windows" => include_str!("catalogs/windows-tools.json"),
        "mobile" => include_str!("catalogs/mobile-tools.json"),
        _ => return Err("invalid native module".into()),
    };
    serde_json::from_str(source).map_err(|e| format!("invalid embedded catalog: {e}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_catalogs_preserve_live_revisions() {
        let revisions: Value =
            serde_json::from_str(include_str!("catalogs/revisions.json")).unwrap();
        for (module, count) in [("windows", 14), ("mobile", 30)] {
            let tools = catalog(module).unwrap();
            assert_eq!(tools.as_array().unwrap().len(), count);
            assert_eq!(
                crate::model::digest(&serde_json::to_vec(&tools).unwrap()),
                revisions[module]
            );
            assert!(!tools
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "Drag" || t["name"] == "mobile_allocate_remote_device"));
        }
        let response = respond("mobile", &json!({"method":"initialize","id":7}))
            .unwrap()
            .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["id"], 7);
    }
}
