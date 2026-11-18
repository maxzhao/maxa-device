//! Owned rotation settings, not a global override of application orientation policy.
use super::{
    transport::{cmd, shell},
    Context, Phone,
};
use crate::model::Result;
use adbutils::AdbDevice;
use serde_json::json;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Settings {
    automatic: u8,
    rotation: u8,
}
pub(super) struct Override {
    original: Settings,
    owned: Vec<Settings>,
}
fn owns_current(state: Option<&Override>, current: Settings) -> bool {
    state.is_some_and(|state| state.owned.contains(&current))
}
async fn observe(dev: &AdbDevice) -> Result<Settings> {
    let automatic = shell(
        dev,
        cmd(&["settings", "get", "system", "accelerometer_rotation"]),
    )
    .await?;
    let rotation = shell(dev, cmd(&["settings", "get", "system", "user_rotation"])).await?;
    let automatic = automatic
        .parse::<u8>()
        .map_err(|_| "UNSUPPORTED: unknown accelerometer_rotation")?;
    let rotation = rotation
        .parse::<u8>()
        .map_err(|_| "UNSUPPORTED: unknown user_rotation")?;
    if automatic > 1 || rotation > 3 {
        return Err("UNSUPPORTED: unknown Android rotation settings".into());
    }
    Ok(Settings {
        automatic,
        rotation,
    })
}
async fn settle(dev: &AdbDevice, expected: Settings) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut stable_since = None;
    loop {
        let now = tokio::time::Instant::now();
        if observe(dev).await? == expected {
            let since = stable_since.get_or_insert(now);
            if since.elapsed() >= Duration::from_millis(500) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if now >= deadline {
            return Err("OUTCOME_UNKNOWN: rotation settings did not stabilize".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
pub(super) async fn execute(ctx: &Context<'_>, mode: &str) -> Result<String> {
    let target = match mode {
        "portrait" => 0,
        "landscape" => 1,
        _ => return Err("invalid orientation".into()),
    };
    let attempt = async {
        let mut slot = ctx.phone.orientation.lock().await;
        ctx.check()?;
        let current = ctx.run(observe(&ctx.dev)).await??;
        if !owns_current(slot.as_ref(), current) {
            *slot = Some(Override { original: current, owned: vec![current] });
        }
        let state = slot.as_mut().unwrap();
        state.owned = vec![current, Settings { automatic: 0, ..current }, Settings { automatic: 0, rotation: target }];
        // Let WindowManager update its own policy and settings together. Separate
        // settings writes can race its observer and re-lock automatic rotation.
        ctx.shell(&["wm", "user-rotation", "lock", &target.to_string()]).await?;
        ctx.run(settle(&ctx.dev, Settings { automatic: 0, rotation: target })).await??;
        ctx.phone.elements.lock().await.clear();
        // Settings observers, display policy and activity relayout are asynchronous.
        // Poll observations only: never resend a rotation or relax app orientation policy.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let info = ctx.rpc("deviceInfo", json!([])).await?;
            let rotation = info["displayRotation"].as_u64().filter(|r| *r <= 3)
                .ok_or("OUTCOME_UNKNOWN: cannot observe display rotation")?;
            if (rotation % 2 == 0) == (mode == "portrait")
                && ctx.run(observe(&ctx.dev)).await?? == (Settings { automatic: 0, rotation: target }) {
                state.owned = vec![Settings { automatic: 0, rotation: target }];
                return Ok::<_, String>(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("UNSUPPORTED: display did not rotate; foreground app or display policy may require a fixed orientation; original settings restored".into());
            }
            ctx.run(tokio::time::sleep(Duration::from_millis(100))).await?;
        }
    }.await;
    if let Err(e) = attempt {
        if let Err(cleanup) = clear(ctx.phone, &ctx.dev).await {
            return Err(format!(
                "OUTCOME_UNKNOWN: orientation failed: {e}; restoration incomplete: {cleanup}"
            ));
        }
        return Err(e);
    }
    Ok(super::tools::orientation_changed(mode))
}
pub(super) async fn clear(phone: &Phone, dev: &AdbDevice) -> Result<()> {
    let mut slot = phone.orientation.lock().await;
    let Some(state) = slot.as_mut() else {
        return Ok(());
    };
    let current = observe(dev).await?;
    if current != state.original {
        if !owns_current(Some(state), current) {
            slot.take();
            return Ok(());
        }
        state.owned.push(Settings {
            automatic: current.automatic,
            rotation: state.original.rotation,
        });
        shell(
            dev,
            cmd(&[
                "wm",
                "user-rotation",
                "lock",
                &state.original.rotation.to_string(),
            ]),
        )
        .await?;
        // Wait for the lock's settings observer before freeing rotation. Otherwise
        // its delayed notification can re-lock the device after the free command.
        settle(
            dev,
            Settings {
                automatic: 0,
                rotation: state.original.rotation,
            },
        )
        .await?;
        if state.original.automatic == 1 {
            shell(dev, cmd(&["wm", "user-rotation", "free"])).await?;
            settle(dev, state.original).await?;
        }
    }
    slot.take();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_requires_registered_current_settings() {
        let original = Settings {
            automatic: 1,
            rotation: 0,
        };
        let owned = Settings {
            automatic: 0,
            rotation: 1,
        };
        let external = Settings {
            automatic: 0,
            rotation: 2,
        };
        let state = Override {
            original,
            owned: vec![owned],
        };
        assert!(owns_current(Some(&state), owned));
        assert!(!owns_current(Some(&state), external));
        assert!(!owns_current(Some(&state), original));
        assert!(!owns_current(None, owned));
    }
}
