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
    let value: Value = serde_json::from_str(source).map_err(|e| format!("invalid embedded catalog: {e}"))?;
    if cfg!(target_os = "android") && module == "mobile" { return Ok(android_subset(&value)); }
    Ok(value)
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

pub fn android_tool(name: &str) -> bool {
    matches!(name, "mobile_list_available_devices" | "mobile_list_apps" | "mobile_get_foreground_app" | "mobile_launch_app" | "mobile_open_url" | "mobile_get_screen_size" | "mobile_get_orientation" | "mobile_set_orientation" | "mobile_list_elements_on_screen" | "mobile_click_on_screen_at_coordinates" | "mobile_double_tap_on_screen" | "mobile_long_press_on_screen_at_coordinates" | "mobile_swipe_on_screen" | "mobile_press_button" | "mobile_type_keys" | "mobile_take_screenshot" | "mobile_save_screenshot" | "mobile_clipboard" | "mobile_install_app" | "mobile_uninstall_app" | "mobile_start_screen_recording" | "mobile_stop_screen_recording" | "mobile_batch_commands")
}
fn android_subset(full: &Value) -> Value {
    Value::Array(full.as_array().unwrap().iter().filter(|t| android_tool(t["name"].as_str().unwrap_or(""))).cloned().collect())
}
#[cfg(test)]
#[test]
fn android_subset_has_23_byte_identical_definitions() {
    let full: Value = serde_json::from_str(include_str!("catalogs/mobile-tools.json")).unwrap();
    let subset=android_subset(&full);
    assert_eq!(subset.as_array().unwrap().len(),23);
    for tool in subset.as_array().unwrap() {
        let source=full.as_array().unwrap().iter().find(|t|t["name"]==tool["name"]).unwrap();
        assert_eq!(serde_json::to_vec(tool).unwrap(),serde_json::to_vec(source).unwrap());
    }
}
