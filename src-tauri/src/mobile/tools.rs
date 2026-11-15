// Behavior port of Mobile-MCP 1.0.17, Apache-2.0 (licenses/mobile-mcp-LICENSE).
use super::{hierarchy, transport, Context, Recording};
use crate::{
    files::Files,
    model::{id, Result},
};
use base64::Engine;
use image::GenericImageView;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().ok_or_else(|| format!("{key} required"))
}
fn text(s: impl Into<String>) -> Vec<Value> {
    vec![json!({"type":"text","text":s.into()})]
}
fn package(args: &Value) -> Result<&str> {
    let p = string(args, "packageName")?;
    if p.is_empty()
        || !p
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
        || p.starts_with('.')
    {
        return Err("invalid Android package name".into());
    }
    Ok(p)
}
fn extension(path: &str, allowed: &[&str]) -> Result<()> {
    let ext = Path::new(path)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !allowed.contains(&ext.as_str()) {
        return Err(format!("unsupported output extension: {ext}"));
    }
    Ok(())
}
// Only state.rs supplies these private staging paths; pin ancestors and refuse overwrite/reparse leaves.
fn write(path: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let path = Path::new(path);
    let _pin = Files::pin(path, true)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())
}
pub(super) async fn dispatch(ctx: &Context<'_>, name: &str, args: &Value) -> Result<Vec<Value>> {
    ctx.check()?;
    let s = match name {
        "mobile_list_apps" => {
            let apps = ctx.package_catalog(None).await?;
            let apps = apps.as_array().ok_or("invalid launchable app list")?;
            let labels = apps
                .iter()
                .map(|app| {
                    Ok(format!(
                        "{} ({})",
                        string(app, "name")?,
                        string(app, "packageName")?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            format!("Found these apps on device: {}", labels.join(", "))
        }
        "mobile_get_foreground_app" => {
            let out = ctx.shell(&["dumpsys", "activity", "activities"]).await?;
            let component = out
                .lines()
                .filter(|l| l.contains("mResumedActivity") || l.contains("topResumedActivity"))
                .flat_map(str::split_whitespace)
                .find(|p| p.contains('/') && !p.starts_with('/'))
                .map(|s| s.trim_end_matches('}'))
                .ok_or("UNSUPPORTED: foreground activity not observable")?;
            let (p, a) = component.split_once('/').ok_or("invalid activity")?;
            let app = ctx.package_catalog(Some(p)).await?;
            let label = string(&app, "name")?;
            let activity = if a.starts_with('.') {
                format!("{p}{a}")
            } else {
                a.into()
            };
            format!("Foreground app: {label} ({p}), activity: {activity}")
        }
        "mobile_launch_app" => {
            let p = package(args)?;
            if let Some(locale) = args["locale"].as_str() {
                if locale.is_empty()
                    || !locale
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b',')
                {
                    return Err("invalid locale tags".into());
                }
                let sdk = ctx
                    .shell(&["getprop", "ro.build.version.sdk"])
                    .await?
                    .parse::<u32>()
                    .unwrap_or(0);
                if sdk < 33 {
                    return Err(
                        "UNSUPPORTED: per-app locale requires Android 13+; no launch sent".into(),
                    );
                }
                ctx.shell(&["cmd", "locale", "set-app-locales", p, "--locales", locale])
                    .await?;
            }
            let resolved = ctx
                .shell(&[
                    "cmd",
                    "package",
                    "resolve-activity",
                    "--brief",
                    "-a",
                    "android.intent.action.MAIN",
                    "-c",
                    "android.intent.category.LAUNCHER",
                    p,
                ])
                .await?;
            let activity = resolved
                .lines()
                .rev()
                .find(|s| s.contains('/') && !s.contains(' '))
                .ok_or("app has no observable launcher activity")?;
            let output = ctx.shell(&["am", "start", "-W", "-n", activity]).await?;
            if output.contains("Error:") || output.contains("Exception") {
                return Err(output);
            }
            format!("Launched app {p}")
        }
        "mobile_terminate_app" => {
            let p = package(args)?;
            ctx.shell(&["am", "force-stop", p]).await?;
            format!("Terminated app {p}")
        }
        "mobile_clear_app_data" => {
            let p = package(args)?;
            let out = ctx.shell(&["pm", "clear", p]).await?;
            success(&out)?;
            format!("Cleared app data for {p}")
        }
        "mobile_install_app" => {
            let path = string(args, "path")?;
            extension(path, &["apk"])?;
            let _pin = Files::pin(Path::new(path), false)?;
            let remote = format!("/data/local/tmp/maxa-install-{}.apk", id());
            ctx.run(ctx.dev.sync().push(Path::new(path), &remote, 0o600, true))
                .await?
                .map_err(|e| e.to_string())?;
            let installed = ctx
                .shell(&["pm", "install", "-r", &remote])
                .await
                .and_then(|out| success(&out));
            let _ = transport::shell(&ctx.dev, transport::cmd(&["rm", "-f", &remote])).await;
            installed?;
            format!("Installed app from {path}")
        }
        "mobile_uninstall_app" => {
            let p = package(args)?;
            let out = ctx.shell(&["pm", "uninstall", p]).await?;
            success(&out)?;
            format!("Uninstalled app {p}")
        }
        "mobile_get_screen_size" => {
            let (w, h) = screen_size(ctx).await?;
            format!("Screen size is {w}x{h} pixels")
        }
        "mobile_click_on_screen_at_coordinates"
        | "mobile_double_tap_on_screen"
        | "mobile_long_press_on_screen_at_coordinates" => {
            let (x, y) = hierarchy::resolve(&ctx.phone.elements.lock().await, args)?;
            if name == "mobile_long_press_on_screen_at_coordinates" {
                let duration = args["duration"].as_f64().unwrap_or(500.).round();
                ctx.action("swipe", json!([x, y, x, y, (duration / 5.).max(1.) as u64]))
                    .await?;
                format!("Long pressed at {x}, {y} for {duration}ms")
            } else {
                ctx.action("click", json!([x, y])).await?;
                if name == "mobile_double_tap_on_screen" {
                    ctx.action("click", json!([x, y])).await?;
                }
                format!(
                    "{} at {x}, {y}",
                    if name == "mobile_double_tap_on_screen" {
                        "Double-tapped"
                    } else {
                        "Clicked"
                    }
                )
            }
        }
        "mobile_list_elements_on_screen" => {
            let mode = args["format"].as_str().unwrap_or("text");
            if !["text", "json", "raw"].contains(&mode) {
                return Err("invalid hierarchy format".into());
            }
            let (elements, raw) = ctx.hierarchy(true).await?;
            if mode == "raw" {
                ctx.phone.elements.lock().await.clear();
                raw.to_string()
            } else {
                hierarchy::format(&elements, mode)
            }
        }
        "mobile_press_button" => {
            let button = string(args, "button")?;
            let code = button_code(button)
                .ok_or_else(|| format!("UNSUPPORTED: Android button {button}"))?;
            ctx.shell(&["input", "keyevent", code]).await?;
            format!("Pressed the button: {button}")
        }
        "mobile_open_url" => {
            let url = string(args, "url")?;
            let (scheme, tail) = url
                .split_once("://")
                .ok_or("Only http:// and https:// URLs allowed")?;
            if !["http", "https"].contains(&scheme.to_ascii_lowercase().as_str()) {
                return Err("Only http:// and https:// URLs allowed".into());
            }
            let url = format!("{}://{tail}", scheme.to_ascii_lowercase());
            let out = ctx
                .shell(&[
                    "am",
                    "start",
                    "-W",
                    "-a",
                    "android.intent.action.VIEW",
                    "-d",
                    &url,
                ])
                .await?;
            if out.contains("Error:") || out.contains("Exception") {
                return Err(out);
            }
            format!("Opened URL: {url}")
        }
        "mobile_swipe_on_screen" => {
            let direction = string(args, "direction")?;
            if args["x"].is_null() != args["y"].is_null() {
                return Err("Provide both x and y, or neither".into());
            }
            let (w, h) = screen_size(ctx).await?;
            let explicit = !args["x"].is_null();
            let distance = if explicit {
                args["distance"].as_f64().unwrap_or(400.)
            } else {
                400.
            };
            let (dx, dy) = match direction {
                "up" => (0., -distance),
                "down" => (0., distance),
                "left" => (-distance, 0.),
                "right" => (distance, 0.),
                _ => return Err("invalid direction".into()),
            };
            let (x, y) = if explicit {
                (
                    args["x"].as_f64().ok_or("invalid x")?,
                    args["y"].as_f64().ok_or("invalid y")?,
                )
            } else {
                (w as f64 / 2. - dx / 2., h as f64 / 2. - dy / 2.)
            };
            ctx.action(
                "swipe",
                json!([
                    x.round() as i32,
                    y.round() as i32,
                    (x + dx).round() as i32,
                    (y + dy).round() as i32,
                    55
                ]),
            )
            .await?;
            format!("Swiped {direction} on screen")
        }
        "mobile_type_keys" => {
            let input = string(args, "text")?;
            type_keys(ctx, input).await?;
            if args["submit"] == true {
                ctx.shell(&["input", "keyevent", "66"]).await?;
            }
            format!("Typed text: {input}")
        }
        "mobile_save_screenshot" | "mobile_take_screenshot" => {
            return screenshot(ctx, name, args).await
        }
        "mobile_get_orientation" => {
            let info = ctx.rpc("deviceInfo", json!([])).await?;
            let r = info["displayRotation"]
                .as_u64()
                .ok_or("UNSUPPORTED: current display rotation unavailable")?;
            format!(
                "Current device orientation is {}",
                if r % 2 == 0 { "portrait" } else { "landscape" }
            )
        }
        "mobile_set_orientation" => {
            return super::orientation::execute(ctx, string(args, "orientation")?)
                .await
                .map(text)
        }
        "mobile_fold_device" => return fold(ctx, args).await.map(text),
        "mobile_set_location" => return super::location::execute(ctx, args).await.map(text),
        "mobile_clipboard" => {
            if let Some(input) = args["text"].as_str() {
                let before = ctx.rpc("getClipboard", json!([])).await?;
                before
                    .as_str()
                    .ok_or("UNSUPPORTED: clipboard not readable; no write sent")?;
                ctx.action("setClipboard", json!(["maxa-device", input]))
                    .await?;
                if ctx.rpc("getClipboard", json!([])).await?.as_str() != Some(input) {
                    return Err(
                        "OUTCOME_UNKNOWN: clipboard write not confirmed; observe before retrying"
                            .into(),
                    );
                }
                "Clipboard updated".into()
            } else {
                ctx.rpc("getClipboard", json!([]))
                    .await?
                    .as_str()
                    .ok_or("UNSUPPORTED: clipboard text unavailable")?
                    .into()
            }
        }
        "mobile_get_device_logs" => return logs(ctx, args).await.map(text),
        "mobile_list_crashes" => return list_crashes(ctx).await.map(text),
        "mobile_get_crash" => {
            let id = string(args, "id")?;
            let raw = ctx
                .shell(&["logcat", "-b", "crash", "-d", "-v", "year"])
                .await?;
            crash_reports(&raw)
                .into_iter()
                .find(|(v, _)| v["id"] == id)
                .map(|(_, report)| report)
                .ok_or_else(|| format!("crash {id} not found"))?
        }
        "mobile_start_screen_recording" => return record_start(ctx, args).await.map(text),
        "mobile_stop_screen_recording" => return record_stop(ctx).await.map(text),
        _ => return Err(format!("UNSUPPORTED: native Mobile tool {name}")),
    };
    ctx.check()?;
    Ok(text(s))
}
fn success(output: &str) -> Result<()> {
    if output.lines().any(|l| l.trim() == "Success") {
        Ok(())
    } else {
        Err(format!("ANDROID_COMMAND_FAILED: {output}"))
    }
}
fn button_code(button: &str) -> Option<&'static str> {
    Some(match button {
        "HOME" => "3",
        "BACK" => "4",
        "VOLUME_UP" => "24",
        "VOLUME_DOWN" => "25",
        "ENTER" => "66",
        "BACKSPACE" => "67",
        "APP_SWITCH" => "187",
        "POWER" => "26",
        "DPAD_CENTER" => "23",
        "DPAD_UP" => "19",
        "DPAD_DOWN" => "20",
        "DPAD_LEFT" => "21",
        "DPAD_RIGHT" => "22",
        _ => return None,
    })
}
async fn screen_size(ctx: &Context<'_>) -> Result<(u32, u32)> {
    let v = ctx.rpc("deviceInfo", json!([])).await?;
    let w = v["displayWidth"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("screen width unavailable")?;
    let h = v["displayHeight"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("screen height unavailable")?;
    Ok((w as u32, h as u32))
}
async fn type_keys(ctx: &Context<'_>, input: &str) -> Result<()> {
    if input.is_empty() {
        return Ok(());
    }
    if input.is_ascii() {
        // Android input text maps %s to spaces; send that literal pair separately.
        for (index, part) in input.split("%s").enumerate() {
            if index > 0 {
                ctx.shell(&["input", "text", "%"]).await?;
                ctx.shell(&["input", "text", "s"]).await?;
            }
            for (i, line) in part.split('\n').enumerate() {
                if i > 0 {
                    ctx.shell(&["input", "keyevent", "66"]).await?;
                }
                for (j, chunk) in line.split('\t').enumerate() {
                    if j > 0 {
                        ctx.shell(&["input", "keyevent", "61"]).await?;
                    }
                    if !chunk.is_empty() {
                        ctx.shell(&["input", "text", &chunk.replace(' ', "%s")])
                            .await?;
                    }
                }
            }
        }
        return Ok(());
    }
    let (before, _) = ctx.hierarchy(false).await?;
    let focused = before
        .iter()
        .find(|e| e.focused_editor())
        .ok_or("Unicode paste requires an observable focused text editor; no input sent")?;
    let old_text = focused.text().to_owned();
    let identifier = focused.attr("resource-id").to_owned();
    let class = focused.attr("class").to_owned();
    let previous = ctx
        .rpc("getClipboard", json!([]))
        .await?
        .as_str()
        .ok_or("UNSUPPORTED: cannot read text clipboard; no input sent")?
        .to_owned();
    let attempt=async {
        ctx.action("setClipboard",json!(["maxa-device",input])).await?;
        ctx.shell(&["input","keyevent","279"]).await?;
        let deadline=tokio::time::Instant::now()+Duration::from_secs(2);
        loop {
            let (after,_)=ctx.hierarchy(false).await?;
            if after.iter().any(|e|e.focused_editor() && e.attr("resource-id")==identifier && e.attr("class")==class && e.text()!=old_text && e.text().contains(input)) {break;}
            if tokio::time::Instant::now()>=deadline {
                return Err("OUTCOME_UNKNOWN: Unicode paste sent but focused text not verified; observe before retrying".into());
            }
            ctx.run(tokio::time::sleep(Duration::from_millis(100))).await?;
        }
        Ok(())
    }.await;
    // Restoration is bounded but deliberately not cancelled: cancellation must not skip this finally.
    let restore = async {
        let now = transport::rpc(&ctx.dev, "getClipboard", json!([])).await?;
        let now = now
            .as_str()
            .ok_or("clipboard unreadable during conditional restoration")?;
        if now == input {
            transport::action(&ctx.dev, "setClipboard", json!(["maxa-device", previous])).await?;
            if transport::rpc(&ctx.dev, "getClipboard", json!([]))
                .await?
                .as_str()
                != Some(previous.as_str())
            {
                return Err("restored clipboard text was not confirmed".into());
            }
        }
        Ok::<_, String>(())
    }
    .await;
    if let Err(e) = restore {
        return Err(format!(
            "OUTCOME_UNKNOWN: Unicode paste/text clipboard restoration failed: {e}"
        ));
    }
    attempt
}
async fn screenshot(ctx: &Context<'_>, name: &str, args: &Value) -> Result<Vec<Value>> {
    let take = name == "mobile_take_screenshot";
    let save = args["saveTo"].as_str();
    if !take {
        extension(save.ok_or("saveTo required")?, &["png", "jpg", "jpeg"])?;
    }
    let mut conn = ctx
        .run(ctx.dev.open_transport(None, None))
        .await?
        .map_err(|e| e.to_string())?;
    ctx.run(conn.send_command("exec:screencap -p"))
        .await?
        .map_err(|e| e.to_string())?;
    ctx.run(conn.check_okay())
        .await?
        .map_err(|e| e.to_string())?;
    let bytes = ctx
        .run(conn.read_until_close_bytes())
        .await?
        .map_err(|e| e.to_string())?;
    let mut image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let original = image.dimensions();
    let max = args["maxSize"]
        .as_u64()
        .map(|v| v.min(u32::MAX as u64) as u32)
        .or(if take && args["scale"].is_null() {
            Some(1024)
        } else {
            None
        });
    let scale = if let Some(max) = max {
        (max as f64 / original.0.max(original.1) as f64).min(1.)
    } else {
        args["scale"].as_f64().unwrap_or(1.)
    };
    let width = (original.0 as f64 * scale).round().max(1.) as u32;
    let height = (original.1 as f64 * scale).round().max(1.) as u32;
    if (width, height) != original {
        image = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
    }
    ctx.check()?;
    let png = !take && save.unwrap().to_ascii_lowercase().ends_with(".png");
    let mut out = std::io::Cursor::new(Vec::new());
    if png {
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, if take { 75 } else { 90 })
            .encode_image(&image.to_rgb8())
            .map_err(|e| e.to_string())?;
    }
    ctx.check()?;
    if let Some(path) = save {
        write(path, &out.into_inner())?;
        return Ok(text(format!("Screenshot saved to: {path}")));
    }
    let mapping = coordinate_mapping((width, height), original);
    Ok(vec![
        json!({"type":"text","text":mapping}),
        json!({"type":"image","mimeType":"image/jpeg","data":base64::engine::general_purpose::STANDARD.encode(out.into_inner())}),
    ])
}
fn coordinate_mapping(image: (u32, u32), screen: (u32, u32)) -> String {
    if image == screen {
        return format!(
            "Screenshot is {}x{} and its coordinates match the screen.",
            image.0, image.1
        );
    }
    let ratio = |a: u32, b: u32| {
        format!("{:.3}", a as f64 / b as f64)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    };
    format!("Screenshot is {}x{}. Screen coordinates are {}x{}. To tap something you see in this screenshot, multiply its x by {} and y by {}.",image.0,image.1,screen.0,screen.1,ratio(screen.0,image.0),ratio(screen.1,image.1))
}
async fn fold(ctx: &Context<'_>, args: &Value) -> Result<String> {
    if !args["angle"].is_null() {
        return Err("UNSUPPORTED: Android shell cannot set a hinge angle".into());
    }
    let posture = string(args, "posture")?;
    let names: &[&str] = match posture {
        "folded" => &["CLOSED", "FOLDED"],
        "half-open" => &["HALF_OPENED", "HALF_OPEN"],
        "open" => &["OPENED", "OPEN"],
        _ => return Err("invalid fold posture".into()),
    };
    let states = ctx
        .shell(&["cmd", "device_state", "print-states"])
        .await
        .map_err(|e| format!("UNSUPPORTED: fold posture control unavailable: {e}"))?;
    let selected = states.lines().find(|l| {
        names
            .iter()
            .any(|n| l.contains(&format!("name='{n}'")) || l.contains(&format!("name={n},")))
    });
    let state = selected
        .and_then(|l| l.split("identifier=").nth(1))
        .map(|s| s.split(|c: char| !c.is_ascii_digit()).next().unwrap_or(""))
        .filter(|s| !s.is_empty())
        .ok_or("UNSUPPORTED: device has no matching documented fold posture")?;
    ctx.shell(&["cmd", "device_state", "state", state]).await?;
    let observed = ctx.shell(&["cmd", "device_state", "state"]).await?;
    if !observed.contains(&format!("identifier={state},")) && observed.trim() != state {
        return Err("OUTCOME_UNKNOWN: fold state change not confirmed".into());
    }
    Ok(format!("Changed device posture to {posture}"))
}
async fn record_start(ctx: &Context<'_>, args: &Value) -> Result<String> {
    let output = string(args, "output")?;
    extension(output, &["mp4"])?;
    let _pin = Files::pin(Path::new(output), true)?;
    let limit = args["timeLimit"].as_f64().unwrap_or(180.);
    if !limit.is_finite() || !(1. ..=180.).contains(&limit) || limit.fract() != 0. {
        return Err(
            "UNSUPPORTED: Android screenrecord requires an integer timeLimit 1..180 seconds".into(),
        );
    }
    let mut slot = ctx.phone.recording.lock().await;
    if slot.is_some() {
        return Err("recording already active".into());
    }
    let remote = format!("/data/local/tmp/maxa-recording-{}.mp4", id());
    let command = transport::cmd(&["screenrecord", "--time-limit", &limit.to_string(), &remote]);
    ctx.check()?;
    let process =
        transport::OwnedProcess::launch(&ctx.dev, &command, "screenrecord", Some(&remote)).await?;
    if let Err(e) = ctx.check() {
        process.terminate(&ctx.dev, 2).await?;
        transport::shell(&ctx.dev, transport::cmd(&["rm", "-f", &remote])).await?;
        return Err(e);
    }
    *slot = Some(Recording {
        process,
        remote,
        output: PathBuf::from(output),
        started: Instant::now(),
    });
    ctx.check()?;
    Ok(format!(
        "Screen recording started. Output will be saved to: {output}"
    ))
}
async fn record_stop(ctx: &Context<'_>) -> Result<String> {
    let mut slot = ctx.phone.recording.lock().await;
    let recording = slot
        .as_ref()
        .ok_or("No active recording found; start a recording first")?;
    recording.process.signal(&ctx.dev, 2).await?;
    for _ in 0..100 {
        ctx.check()?;
        if recording.process.gone(&ctx.dev).await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !recording.process.gone(&ctx.dev).await? {
        return Err("OUTCOME_UNKNOWN: screenrecord has not finalized; no replay".into());
    }
    let info = ctx
        .run(ctx.dev.sync().stat(&recording.remote))
        .await?
        .map_err(|e| e.to_string())?;
    if info.size == 0 || info.size > 512 * 1024 * 1024 {
        return Err("recording missing/empty or exceeds 512 MiB".into());
    }
    let bytes = ctx
        .run(ctx.dev.sync().read_bytes(&recording.remote))
        .await?
        .map_err(|e| e.to_string())?;
    let output = recording.output.to_string_lossy();
    write(&output, &bytes)?;
    let duration = recording.started.elapsed().as_secs();
    let _ = transport::shell(&ctx.dev, transport::cmd(&["rm", "-f", &recording.remote])).await;
    let answer = format!(
        "Recording stopped. File: {output} ({:.2} MB, ~{duration}s)",
        bytes.len() as f64 / 1048576.
    );
    slot.take();
    Ok(answer)
}
// Behavior-only reimplementation of the Android crash-buffer contract observed in
// mobilecli 1.0.23 (88dd806c, devices/android_crash.go); no upstream FSL code used.
async fn list_crashes(ctx: &Context<'_>) -> Result<String> {
    let raw = ctx
        .shell(&["logcat", "-b", "crash", "-d", "-v", "year"])
        .await?;
    Ok(serde_json::to_string(
        &crash_reports(&raw)
            .into_iter()
            .map(|(v, _)| v)
            .collect::<Vec<_>>(),
    )
    .map_err(|e| e.to_string())?)
}
struct AndroidLog<'a> {
    date: &'a str,
    time: &'a str,
    pid: &'a str,
    level: &'a str,
    tag: &'a str,
    message: &'a str,
}
fn android_log(line: &str) -> Option<AndroidLog<'_>> {
    static LINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(\d{4}-\d{2}-\d{2})\s+(\d{2}:\d{2}:\d{2}\.\d{3})\s+(\d+)\s+\d+\s+(\w)\s+(\S+)\s*: (.*)$").unwrap()
    });
    let c = LINE.captures(line.trim_end_matches(['\r', '\n']))?;
    Some(AndroidLog {
        date: c.get(1)?.as_str(),
        time: c.get(2)?.as_str(),
        pid: c.get(3)?.as_str(),
        level: c.get(4)?.as_str(),
        tag: c.get(5)?.as_str(),
        message: c.get(6)?.as_str(),
    })
}
fn crash_reports(raw: &str) -> Vec<(Value, String)> {
    let lines = raw.split_inclusive('\n').collect::<Vec<_>>();
    let mut reports = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(first) = android_log(lines[i]) else {
            i += 1;
            continue;
        };
        let native =
            first.level == "F" && first.tag == "DEBUG" && first.message.contains("*** ***");
        let java = first.level == "E"
            && first.tag == "AndroidRuntime"
            && first.message.starts_with("FATAL EXCEPTION:");
        if !native && !java {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut name = String::new();
        let mut stack_owner = None;
        let mut crashed_pid = None;
        while i < lines.len() {
            let Some(line) = android_log(lines[i]) else {
                break;
            };
            if line.pid != first.pid || line.tag != first.tag {
                break;
            }
            if native {
                if let Some((_, v)) = line.message.split_once("Cmdline:") {
                    name = v.trim().into();
                }
                if crashed_pid.is_none() {
                    crashed_pid = line
                        .message
                        .split_once("pid:")
                        .and_then(|(_, v)| {
                            v.trim_start().split(|c: char| !c.is_ascii_digit()).next()
                        })
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned);
                }
            } else {
                if let Some(v) = line.message.strip_prefix("Process: ") {
                    name = v.split(',').next().unwrap_or("").trim().into();
                }
                if stack_owner.is_none() {
                    stack_owner = line
                        .message
                        .trim()
                        .strip_prefix("at ")
                        .and_then(|s| s.split_once('('))
                        .and_then(|(s, _)| s.rsplit_once('.'))
                        .map(|(owner, _)| owner.to_owned());
                }
            }
            i += 1;
        }
        if java && name.is_empty() {
            name = stack_owner.unwrap_or_default();
        }
        let pid = crashed_pid.as_deref().unwrap_or(first.pid);
        reports.push((
            json!({"processName":name,"timestamp":format!("{} {}",first.date,first.time),
            "id":format!("{}_{}_{}",first.date,first.time,pid)}),
            lines[start..i]
                .concat()
                .trim_end_matches(['\r', '\n'])
                .into(),
        ));
    }
    reports
}
#[derive(Debug)]
struct LogFilter {
    key: String,
    value: String,
    exclude: bool,
}
fn filters(args: &Value) -> Result<Vec<LogFilter>> {
    let mut result = Vec::new();
    for f in args["filter"].as_array().into_iter().flatten() {
        let f = f.as_str().ok_or("log filter must be string")?;
        let (key, value) = f.split_once('=').ok_or("invalid log filter")?;
        let exclude = key.ends_with('!');
        let key = key.trim_end_matches('!');
        if ![
            "pid",
            "process",
            "tag",
            "level",
            "subsystem",
            "category",
            "message",
        ]
        .contains(&key)
        {
            return Err(format!("UNSUPPORTED: Android log filter key {key}"));
        }
        if value.is_empty() {
            return Err("empty log filter".into());
        }
        result.push(LogFilter {
            key: key.into(),
            value: value.into(),
            exclude,
        });
    }
    Ok(result)
}
fn log_entry(line: &str, processes: &HashMap<String, String>) -> Option<Value> {
    let l = android_log(line)?;
    let level = match l.level {
        "V" => "Verbose",
        "D" => "Debug",
        "I" => "Info",
        "W" => "Warning",
        "E" => "Error",
        "F" => "Fatal",
        "A" => "Assert",
        s => s,
    };
    Some(
        json!({"timestamp":format!("{} {}",l.date,l.time),"pid":l.pid.parse::<u32>().ok()?,
        "level":level,"tag":l.tag,"message":l.message,"process":processes.get(l.pid).cloned().unwrap_or_default()}),
    )
}
async fn logs(ctx: &Context<'_>, args: &Value) -> Result<String> {
    let filters = filters(args)?;
    let limit = args["limit"].as_u64().unwrap_or(100) as usize;
    let path = args["saveTo"].as_str();
    if let Some(path) = path {
        extension(path, &["log", "txt", "jsonl"])?;
    }
    let mut processes = HashMap::<String, String>::new();
    let start = ctx.shell(&["date", "+%Y-%m-%d %H:%M:%S.%3N"]).await?;
    if android_log(&format!("{start} 1 1 I Clock: now")).is_none() {
        return Err("UNSUPPORTED: millisecond Android log clock unavailable".into());
    }
    let mut conn = ctx
        .run(ctx.dev.shell_stream("logcat -v threadtime,year -T 1"))
        .await?
        .map_err(|e| e.to_string())?;
    conn.set_timeout(None);
    let mut buffer = Vec::new();
    let mut output = Vec::new();
    // Old MCP resets its silence timer only on emitted (filtered) JSONL, not
    // unrelated logcat noise. Return partial results after 30 seconds of silence.
    let mut silence = tokio::time::Instant::now() + Duration::from_secs(30);
    'collect: while output.len() < limit {
        let chunk = ctx
            .run(tokio::time::timeout_at(silence, conn.read_some(8192)))
            .await?;
        let bytes = match chunk {
            Err(_) => break,
            Ok(Ok(b)) if b.is_empty() => break,
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(format!("OUTCOME_UNKNOWN: logcat disconnected: {e}")),
        };
        buffer.extend(bytes);
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..=end);
            if line.to_ascii_lowercase().contains("permission denied") {
                return Err("UNSUPPORTED: Android denies shell log access".into());
            }
            if let Some(parsed) = android_log(&line) {
                if !processes.contains_key(parsed.pid) {
                    let name = ctx
                        .run(tokio::time::timeout_at(
                            silence,
                            transport::shell(
                                &ctx.dev,
                                transport::cmd(&["cat", &format!("/proc/{}/cmdline", parsed.pid)]),
                            ),
                        ))
                        .await?;
                    let name = match name {
                        Err(_) => break 'collect,
                        Ok(Ok(s)) => s
                            .split('\0')
                            .next()
                            .unwrap_or("")
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_owned(),
                        Ok(Err(_)) => String::new(),
                    };
                    processes.insert(parsed.pid.into(), name);
                }
            }
            if let Some(entry) = log_entry(&line, &processes) {
                if entry["timestamp"].as_str().unwrap_or("") < start.as_str() {
                    continue;
                }
                if filters.iter().all(|f| {
                    let value = if f.key == "pid" {
                        entry["pid"].to_string()
                    } else {
                        entry[&f.key].as_str().unwrap_or("").into()
                    };
                    let matches = if f.key == "level" {
                        value.eq_ignore_ascii_case(&f.value)
                    } else if f.key == "pid" {
                        value == f.value
                    } else {
                        value.contains(&f.value)
                    };
                    matches != f.exclude
                }) {
                    output.push(entry.to_string());
                    silence = tokio::time::Instant::now() + Duration::from_secs(30);
                    if output.len() == limit {
                        break;
                    }
                }
            }
        }
        if buffer.len() > 1024 * 1024 {
            return Err("log entry exceeds 1 MiB".into());
        }
    }
    let logs = output.join("\n");
    if let Some(path) = path {
        write(path, (logs + "\n").as_bytes())?;
        Ok(format!("Saved {} log entries to: {path}", output.len()))
    } else {
        Ok(if logs.is_empty() {
            "No log entries received".into()
        } else {
            logs
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crash_buffer_reports_use_target_pid_and_keep_raw_text() {
        let raw = "2026-11-14 21:31:20.948 16074 16074 F DEBUG   : *** *** ***\r\n2026-11-14 21:31:20.948 16074 16074 F DEBUG   : Cmdline: /system/bin/sample --arg\r\n2026-11-14 21:31:20.948 16074 16074 F DEBUG   : pid: 6695, tid: 6695\r\n2026-11-16 02:00:00.001 123 123 E AndroidRuntime: FATAL EXCEPTION: main\r\n2026-11-16 02:00:00.001 123 123 E AndroidRuntime: Process: example.app, PID: 123\r\n";
        let reports = crash_reports(raw);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].0["id"], "2026-11-14_21:31:20.948_6695");
        assert_eq!(reports[0].0["processName"], "/system/bin/sample --arg");
        assert!(reports[0].1.contains("\r\n"));
        assert!(!reports[0].1.contains("AndroidRuntime"));
        assert_eq!(reports[1].0["processName"], "example.app");
        assert_eq!(reports[1].0["id"], "2026-11-16_02:00:00.001_123");
    }
    #[test]
    fn screenshot_mapping_and_log_fields() {
        assert!(coordinate_mapping((512, 1024), (1220, 2440)).contains("multiply its x by 2.383"));
        assert!(coordinate_mapping((1220, 2440), (1220, 2440)).contains("coordinates match"));
        let e = log_entry(
            "2026-11-16 02:26:33.695  123  456 E TestTag: a b 中文",
            &HashMap::from([("123".into(), "app".into())]),
        )
        .unwrap();
        assert_eq!(e["tag"], "TestTag");
        assert_eq!(e["pid"], 123);
        assert_eq!(e["timestamp"], "2026-11-16 02:26:33.695");
        assert_eq!(e["message"], "a b 中文");
        assert_eq!(e["process"], "app");
        assert!(filters(&json!({"filter":["tag=X","level!=Error"]})).is_ok());
        assert!(filters(&json!({"filter":["subsystem=x"]})).is_ok());
    }
}
