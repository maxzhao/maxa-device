// Non-UI process/permission checks, run by cargo test --lib --no-default-features.
use crate::{model::*, state::AppState};
use serde_json::json;
pub(crate) async fn fixture() -> (
    std::path::PathBuf,
    std::sync::Arc<AppState>,
    ClientInfo,
    Invocation,
) {
    let root = std::env::temp_dir().join(id());
    let app = AppState::open(root.clone(), root.join("resources")).unwrap();
    let token = id();
    app.store.add_client("test", &token).unwrap();
    let client = app.store.authenticate(&token).unwrap();
    app.jobs.set_enabled(true).await;
    #[cfg(windows)]
    let (command, args) = (
        "cmd.exe",
        json!(["/d", "/s", "/c", "ping -n 30 127.0.0.1 >nul"]),
    );
    #[cfg(not(windows))]
    let (command, args) = ("/bin/sleep", json!(["30"]));
    let call = Invocation {
        request_id: id(),
        chat_id: "owner-chat".into(),
        operation: "process.start".into(),
        arguments: json!({"command":command,"args":args,"cwd":root,"timeout":60}),
    };
    (root, app, client, call)
}
#[tokio::test]
async fn control_stop_preserves_system_and_revoke_cancels_owned_tree() {
    let (root, app, client, call) = fixture().await;
    let launched = app.jobs.launch(&client, &call).await.unwrap();
    let job = launched["jobId"].as_str().unwrap();
    app.stop(Some(&client.id)).await;
    assert_eq!(
        app.store.job(job, &client.id, &call.chat_id).unwrap().state,
        "running"
    );
    app.revoke(&client.id).await.unwrap();
    assert!(app
        .jobs
        .launch(
            &client,
            &Invocation {
                request_id: id(),
                ..call.clone()
            }
        )
        .await
        .is_err());
    let final_state = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            let state = app.store.job(job, &client.id, &call.chat_id).unwrap().state;
            if state != "running" && state != "cancel_requested" {
                break state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(final_state, "cancelled");
    assert!(app
        .jobs
        .output(&client.id, "other-chat", job, "stdout")
        .await
        .is_err());
    app.jobs.set_enabled(false).await;
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn module_barrier_and_startup_failure_leave_no_running_process() {
    let (root, app, client, call) = fixture().await;
    app.jobs.set_enabled(false).await;
    assert!(app
        .jobs
        .launch(&client, &call)
        .await
        .unwrap_err()
        .starts_with("MODULE_UNAVAILABLE"));
    assert!(app.store.jobs().is_empty());
    app.jobs.set_enabled(true).await;
    std::fs::write(root.join("jobs"), b"not a directory").unwrap();
    assert!(app.jobs.launch(&client, &call).await.is_err());
    assert_eq!(app.store.jobs()[0].state, "failed");
    assert_eq!(
        app.jobs
            .status(&client.id, &call.chat_id, &app.store.jobs()[0].id)
            .await
            .unwrap()["state"],
        "failed"
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}
