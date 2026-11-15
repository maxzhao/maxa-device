//! Native Android test-provider commands (AOSP LocationShellCommand), not copied mobilecli code.
//! No APK, root, helper extension, or device-wide location-switch change.
use super::{
    transport::{cmd, shell},
    Context, Phone,
};
use crate::model::{id, Result};
use adbutils::AdbDevice;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

const PROVIDERS: [&str; 3] = ["gps", "network", "fused"];
const PACKAGE: &str = "com.android.shell";
const OP: &str = "android:mock_location";

pub(super) struct Override {
    marker: String,
    previous_mode: String,
    coordinates: (f64, f64),
    publisher: Option<JoinHandle<()>>,
    error: Option<String>,
}
#[derive(Default, Debug)]
struct Provider {
    mock: bool,
    tags: Vec<String>,
    coordinates: Option<(f64, f64)>,
}
// Fail closed on unknown dumpsys formats. Do not confuse "last location" with the injected fix.
fn providers(dump: &str) -> Result<HashMap<String, Provider>> {
    let mut result = HashMap::new();
    let mut current: Option<(String, usize)> = None;
    for line in dump.lines() {
        let text = line.trim();
        let indent = line.len() - line.trim_start().len();
        if let Some((name, suffix)) = text.split_once(" provider") {
            if PROVIDERS.contains(&name) && [":", " [mock]:"].contains(&suffix) {
                result.insert(
                    name.to_owned(),
                    Provider {
                        mock: suffix == " [mock]:",
                        ..Provider::default()
                    },
                );
                current = Some((name.to_owned(), indent));
                continue;
            }
        }
        if let Some((name, base)) = current.as_ref() {
            if !text.is_empty() && indent <= *base {
                current = None;
                continue;
            }
            let view = result.get_mut(name).unwrap();
            if let Some(tags) = text.strip_prefix("extra attribution tags=").and_then(|s| {
                s.strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .or_else(|| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            }) {
                view.tags = tags.split(',').map(|s| s.trim().to_owned()).collect();
            }
            if let Some(location) = text.strip_prefix("last mock location=Location[") {
                view.coordinates = location
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.split_once(','))
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
            }
        }
    }
    if result.is_empty() {
        return Err("UNSUPPORTED: cannot inspect Android location-provider ownership".into());
    }
    Ok(result)
}
fn mode(output: &str) -> Result<String> {
    let mut uid_mode_only = false;
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.trim_start().starts_with("uid mode:") {
            uid_mode_only = true;
            continue;
        }
        if let Some((_, value)) = lower.split_once("mock_location:") {
            let value = value.trim().split([';', ' ', '\n']).next().unwrap_or("");
            if ["allow", "ignore", "deny", "default", "foreground"].contains(&value) {
                return Ok(value.into());
            }
        }
    }
    if output.contains("No operations.") || uid_mode_only {
        return Ok("default".into());
    }
    Err("UNSUPPORTED: cannot snapshot shell mock_location app-op; no change sent".into())
}
async fn appop(dev: &AdbDevice) -> Result<String> {
    mode(&shell(dev, cmd(&["appops", "get", PACKAGE, OP])).await?)
}
async fn observe(dev: &AdbDevice) -> Result<HashMap<String, Provider>> {
    providers(&shell(dev, "dumpsys location".into()).await?)
}
fn owned(view: &Provider, marker: &str) -> bool {
    view.mock && view.tags.iter().any(|tag| tag == marker)
}
fn ensure_owned(views: &HashMap<String, Provider>, marker: &str) -> Result<()> {
    if PROVIDERS
        .iter()
        .all(|p| views.get(*p).map(|v| owned(v, marker)).unwrap_or(false))
    {
        return Ok(());
    }
    Err("ANDROID_LOCATION_OWNERSHIP_UNKNOWN: mock-provider ownership changed; no replacement or replay".into())
}
async fn publish(dev: &AdbDevice, state: &Override, cancelled: &AtomicBool) -> Result<()> {
    ensure_owned(&observe(dev).await?, &state.marker)?;
    let fix = format!("{},{}", state.coordinates.0, state.coordinates.1);
    for provider in PROVIDERS {
        if cancelled.load(Ordering::SeqCst) {
            return Err("CONTROL_STOPPED during location update".into());
        }
        shell(
            dev,
            cmd(&[
                "cmd",
                "location",
                "providers",
                "set-test-provider-location",
                provider,
                "--location",
                &fix,
                "--accuracy",
                "1",
            ]),
        )
        .await?;
    }
    let views = observe(dev).await?;
    ensure_owned(&views, &state.marker)?;
    if !PROVIDERS.iter().all(|p| {
        views[*p]
            .coordinates
            .map(|(a, b)| {
                (a - state.coordinates.0).abs() <= 0.000001
                    && (b - state.coordinates.1).abs() <= 0.000001
            })
            .unwrap_or(false)
    }) {
        return Err(
            "OUTCOME_UNKNOWN: mock coordinates were not confirmed; no automatic retry".into(),
        );
    }
    Ok(())
}
fn start_publisher(phone: Arc<Phone>, marker: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if phone.cancelled.load(Ordering::SeqCst) {
                break;
            }
            let mut slot = phone.location.lock().await;
            let Some(state) = slot.as_mut().filter(|s| s.marker == marker) else {
                break;
            };
            let dev = phone.dev.lock().unwrap().clone();
            if let Err(e) = publish(&dev, state, &phone.cancelled).await {
                // Periodic updates are requested behavior, not recovery: any failure ends the loop.
                state.error = Some(e);
                break;
            }
        }
    })
}
pub(super) async fn execute(ctx: &Context<'_>, args: &Value) -> Result<String> {
    if args["latitude"].is_null() && args["longitude"].is_null() {
        clear(ctx.phone, &ctx.dev).await?;
        return Ok("Cleared location override".into());
    }
    let lat = args["latitude"]
        .as_f64()
        .ok_or("Both latitude and longitude required")?;
    let lon = args["longitude"]
        .as_f64()
        .ok_or("Both latitude and longitude required")?;
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return Err("latitude/longitude out of range; no location change sent".into());
    }
    let attempt = async {
        let mut slot = ctx.phone.location.lock().await;
        ctx.check()?;
        if slot.is_none() {
            // Android's LocationShellCommand prints valid help through handleDefaultCommands
            // but returns -1 (ADB exit 255). Accept that status only for this read-only probe.
            let help = ctx.run(ctx.dev.shell2(cmd(&["cmd", "location", "help"]), true)).await?
                .map_err(|e| format!("UNSUPPORTED: cannot query location shell API: {e}"))?;
            if ![0, 255].contains(&help.returncode)
                || !help.output.trim_start().starts_with("Location service commands:")
                || !["add-test-provider", "remove-test-provider", "set-test-provider-enabled", "set-test-provider-location", "--extraAttributionTags"].iter().all(|s|help.output.contains(s)) {
                return Err("UNSUPPORTED: Android lacks the required test-provider shell API; no APK/root fallback".into());
            }
            let views = ctx.run(observe(&ctx.dev)).await??;
            if views.values().any(|v|v.mock) {
                return Err("ANDROID_CONTROL_BUSY: existing mock location requires human handover; no changes sent".into());
            }
            let previous_mode = ctx.run(appop(&ctx.dev)).await??;
            // Register cleanup state before the first mutation; cancellation must retain its owner.
            *slot = Some(Override { marker: format!("maxa-location-{}", id()), previous_mode,
                coordinates: (lat,lon), publisher: None, error: None });
            ctx.shell(&["appops", "set", PACKAGE, OP, "allow"]).await?;
            if ctx.run(appop(&ctx.dev)).await?? != "allow" { return Err("UNSUPPORTED: shell mock_location authorization denied".into()); }
            let marker = slot.as_ref().unwrap().marker.clone();
            for provider in PROVIDERS {
                if observe(&ctx.dev).await?.get(provider).map(|v|v.mock).unwrap_or(false) {
                    return Err("ANDROID_CONTROL_BUSY: mock location changed during setup; no replacement sent".into());
                }
                ctx.shell(&["cmd", "location", "providers", "add-test-provider", provider,
                    "--supportsAltitude", "--supportsSpeed", "--supportsBearing", "--extraAttributionTags", &marker]).await?;
                // AOSP can silently no-op when the app-op is denied. Exit status is not success evidence.
                let views = ctx.run(observe(&ctx.dev)).await??;
                if !views.get(provider).map(|v|owned(v,&marker)).unwrap_or(false) {
                    return Err("UNSUPPORTED: test-provider registration not confirmed; no location success claimed".into());
                }
                ctx.shell(&["cmd", "location", "providers", "set-test-provider-enabled", provider, "true"]).await?;
            }
        }
        let state = slot.as_mut().unwrap();
        if let Some(error) = state.error.as_ref() {
            return Err(format!("OUTCOME_UNKNOWN: previous location publisher stopped: {error}; clear before a new set"));
        }
        // The publisher holds this same lock while sending: it is idle here, not killed mid-input.
        if let Some(publisher) = state.publisher.take() { publisher.abort(); }
        state.coordinates = (lat,lon);
        ctx.run(publish(&ctx.dev, state, &ctx.phone.cancelled)).await??;
        ctx.check()?;
        state.publisher = Some(start_publisher(Arc::clone(ctx.phone), state.marker.clone()));
        Ok::<_,String>(())
    }.await;
    if let Err(e) = attempt {
        // Cleanup is bounded by transport timeouts and deliberately ignores the cancelled call token.
        if let Err(cleanup) = clear(ctx.phone, &ctx.dev).await {
            return Err(format!(
                "OUTCOME_UNKNOWN: location set failed: {e}; cleanup incomplete: {cleanup}"
            ));
        }
        return Err(e);
    }
    Ok(format!("Set device location to {lat},{lon}"))
}
pub(super) async fn clear(phone: &Phone, dev: &AdbDevice) -> Result<()> {
    let mut slot = phone.location.lock().await;
    let Some(state) = slot.as_mut() else {
        return Ok(());
    };
    if let Some(publisher) = state.publisher.take() {
        publisher.abort();
    }
    for provider in PROVIDERS {
        let views = observe(dev).await?;
        match views.get(provider) {
            Some(v) if owned(v,&state.marker) => {
                shell(dev, cmd(&["cmd", "location", "providers", "remove-test-provider", provider])).await?;
                if observe(dev).await?.get(provider).map(|v|v.mock).unwrap_or(false) {
                    return Err("OUTCOME_UNKNOWN: owned mock-provider removal was not confirmed".into());
                }
            }
            Some(v) if v.mock => return Err("ANDROID_LOCATION_OWNERSHIP_UNKNOWN: foreign mock provider preserved; app-op not reset; human handover required".into()),
            _ => {}
        }
    }
    // Do not overwrite a permission change made after our grant.
    if appop(dev).await? == "allow" && state.previous_mode != "allow" {
        shell(
            dev,
            cmd(&["appops", "set", PACKAGE, OP, &state.previous_mode]),
        )
        .await?;
        if appop(dev).await? != state.previous_mode {
            return Err("OUTCOME_UNKNOWN: mock_location app-op restoration not confirmed".into());
        }
    }
    slot.take();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_snapshot_and_provider_ownership_are_explicit() {
        assert_eq!(mode("MOCK_LOCATION: allow; time=+1s").unwrap(), "allow");
        assert_eq!(
            mode("No operations.\nDefault mode: default").unwrap(),
            "default"
        );
        assert_eq!(mode("Uid mode: MOCK_LOCATION: allow").unwrap(), "default");
        assert_eq!(
            mode("Uid mode: MOCK_LOCATION: allow\nMOCK_LOCATION: ignore").unwrap(),
            "ignore"
        );
        assert!(mode("permission denied").is_err());
        let dump="Location Provider Managers:\n  gps provider [mock]:\n    allowed=true\n    extra attribution tags=[maxa-location-one]\n    last mock location=Location[gps -1.234567,2.345678 hAcc=1.0 mock]\n  network provider:\n    allowed=true\nOther section:\n  extra attribution tags=[wrong]\n";
        let views = providers(dump).unwrap();
        assert!(owned(&views["gps"], "maxa-location-one"));
        let braces =
            providers(&dump.replace("tags=[maxa-location-one]", "tags={maxa-location-one}"))
                .unwrap();
        assert!(owned(&braces["gps"], "maxa-location-one"));
        assert!(!owned(&views["gps"], "maxa-location-on"));
        assert_eq!(views["gps"].coordinates, Some((-1.234567, 2.345678)));
        assert!(!views["network"].mock);
        assert!(providers("Permission Denial").is_err());
        assert!(ensure_owned(&views, "maxa-location-one").is_err());
    }
}
