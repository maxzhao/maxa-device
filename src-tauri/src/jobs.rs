use crate::{model::*, modules::command, store::Store};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{oneshot, Mutex},
};

pub struct Jobs {
    store: Arc<Store>,
    root: PathBuf,
    artifacts: Arc<crate::artifacts::Artifacts>,
    running: Mutex<Running>,
}
struct Running {
    enabled: bool,
    entries: HashMap<String, (String, Option<oneshot::Sender<()>>)>,
}
#[cfg(windows)]
pub(crate) struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
impl ProcessTree {
    fn terminate(&self) -> bool {
        use windows_sys::Win32::System::JobObjects::*;
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return false;
        }
        for _ in 0..100 {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as _,
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            } != 0
                && accounting.ActiveProcesses == 0
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }
}
#[cfg(windows)]
pub(crate) fn own_tree(child: &tokio::process::Child) -> Result<ProcessTree> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::{
        Foundation::*,
        System::{JobObjects::*, Threading::*},
    };
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let handle = ProcessTree(job);
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let process = unsafe {
        OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_SUSPEND_RESUME,
            false as _,
            child.id().ok_or("missing process id")?,
        )
    };
    if process.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let assigned = unsafe { AssignProcessToJobObject(job, process) };
    unsafe {
        CloseHandle(process);
    }
    if assigned == 0 {
        return Err("could not assign owned Windows process tree".into());
    }
    Ok(handle)
}
#[cfg(windows)]
pub(crate) fn resume_tree(child: &tokio::process::Child) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME},
    };
    #[link(name = "ntdll")]
    extern "system" {
        fn NtResumeProcess(handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }
    let process = unsafe {
        OpenProcess(
            PROCESS_SUSPEND_RESUME,
            0,
            child.id().ok_or("missing process id")?,
        )
    };
    if process.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let status = unsafe { NtResumeProcess(process) };
    unsafe {
        CloseHandle(process);
    }
    if status < 0 {
        return Err("could not resume owned Windows process tree".into());
    }
    Ok(())
}
impl Jobs {
    pub fn new(
        store: Arc<Store>,
        root: PathBuf,
        artifacts: Arc<crate::artifacts::Artifacts>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            root,
            artifacts,
            running: Mutex::new(Running {
                enabled: false,
                entries: HashMap::new(),
            }),
        })
    }
    pub async fn launch(self: &Arc<Self>, client: &ClientInfo, call: &Invocation) -> Result<Value> {
        // This barrier covers permission recheck, spawn, ownership registration and resume.
        // Revoke/module-close use the same lock; a starting process cannot escape their scan.
        let mut running = self.running.lock().await;
        if !running.enabled {
            return Err("MODULE_UNAVAILABLE: system".into());
        }
        self.store.active(&client.id)?;
        let a = &call.arguments;
        let timeout = match a.get("timeout").filter(|v| !v.is_null()) {
            Some(v) => v.as_f64().ok_or("timeout must be numeric")?,
            None => 300.0,
        };
        if !timeout.is_finite() {
            return Err("timeout must be finite".into());
        }
        let cwd = Path::new(
            a.get("cwd")
                .and_then(Value::as_str)
                .ok_or("absolute cwd required")?,
        );
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err("cwd must be an accessible absolute target directory".into());
        }
        let program = a
            .get("command")
            .and_then(Value::as_str)
            .ok_or("command required")?;
        if program.trim().is_empty() {
            return Err("command must be nonblank".into());
        }
        let options = a.get("opts").cloned().unwrap_or(json!({}));
        if !options.is_object() {
            return Err("opts must be object".into());
        }
        for (k, v) in options.as_object().unwrap() {
            if !matches!(k.as_str(), "async" | "detach") || (!v.is_boolean() && !v.is_null()) {
                return Err(format!("unsupported run option {k}"));
            }
        }
        let detach = options["detach"] == true;
        if detach && options["async"] == true {
            return Err("async/detach are mutually exclusive".into());
        }
        let mut cmd = if let Some(args) = a.get("args").filter(|v| !v.is_null()) {
            let args = args.as_array().ok_or("literal args must be array")?;
            let mut c = command(program);
            for value in args {
                c.arg(value.as_str().ok_or("argv entries must be strings")?);
            }
            c
        } else {
            if cwd.to_string_lossy().starts_with("\\\\") {
                return Err("cmd-string UNC cwd is unsupported; use native argv".into());
            }
            #[cfg(windows)]
            let shell = std::env::var("COMSPEC").unwrap_or("cmd.exe".into());
            #[cfg(not(windows))]
            let shell = "/bin/sh".to_string();
            let mut c = command(shell);
            #[cfg(windows)]
            c.args(["/d", "/s", "/c"]);
            #[cfg(not(windows))]
            c.arg("-c");
            c.arg(program);
            c
        };
        cmd.current_dir(cwd).stdin(std::process::Stdio::null());
        if detach {
            cmd.kill_on_drop(false)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            cmd.creation_flags(0x00000200);
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            return Ok(
                json!({"state":"launched","pid":child.id(),"hostProcessIdentity":id(),"detached":true}),
            );
        }
        #[cfg(windows)]
        cmd.creation_flags(0x08000000 | 0x00000004);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let job = self.store.new_job(&client.id, call)?;
        let dir = self.root.join("jobs").join(&job.id);
        let prepared = (|| -> Result<_> {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let out = std::fs::File::create(dir.join("stdout")).map_err(|e| e.to_string())?;
            let err = std::fs::File::create(dir.join("stderr")).map_err(|e| e.to_string())?;
            Ok((out, err))
        })();
        let (out, err) = match prepared {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                self.store.job_state(&job.id, "failed", None)?;
                return Err(e);
            }
        };
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                drop((out, err));
                let _ = std::fs::remove_dir_all(&dir);
                self.store.job_state(&job.id, "failed", None)?;
                return Err(e.to_string());
            }
        };
        #[cfg(windows)]
        let tree = match own_tree(&child) {
            Ok(tree) => tree,
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                drop((out, err));
                let _ = std::fs::remove_dir_all(&dir);
                self.store.job_state(&job.id, "failed", None)?;
                return Err(e);
            }
        };
        let (tx, rx) = oneshot::channel();
        running
            .entries
            .insert(job.id.clone(), (client.id.clone(), Some(tx)));
        let started = (|| -> Result<()> {
            self.store.job_state(&job.id, "running", None)?;
            #[cfg(windows)]
            resume_tree(&child)?;
            Ok(())
        })();
        if let Err(e) = started {
            running.entries.remove(&job.id);
            #[cfg(windows)]
            drop(tree);
            let _ = child.kill().await;
            let _ = child.wait().await;
            drop((out, err));
            let _ = std::fs::remove_dir_all(&dir);
            self.store.job_state(&job.id, "failed", None)?;
            return Err(e);
        }
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out_task = tokio::spawn(capture(stdout, tokio::fs::File::from_std(out)));
        let err_task = tokio::spawn(capture(stderr, tokio::fs::File::from_std(err)));
        let this = self.clone();
        let jid = job.id.clone();
        tokio::spawn(async move {
            #[cfg(windows)]
            let mut tree = Some(tree);
            let timer = async {
                if timeout > 0.0 {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(timeout)).await
                } else {
                    std::future::pending::<()>().await
                }
            };
            let (state, code) = tokio::select! {
             exit=child.wait()=>match exit{Ok(s)=>(if s.success(){"succeeded"}else{"failed"},s.code()),Err(_)=>("unknown",None)},
             _=rx=>{#[cfg(windows)]let tree_stopped=tree.as_ref().map(|t|t.terminate()).unwrap_or(false);#[cfg(not(windows))]let tree_stopped=true;let _=child.kill().await;let confirmed=child.wait().await.is_ok();(if tree_stopped&&confirmed{"cancelled"}else{"unknown"},None)},
             _=timer=>{#[cfg(windows)]let tree_stopped=tree.as_ref().map(|t|t.terminate()).unwrap_or(false);#[cfg(not(windows))]let tree_stopped=true;let _=child.kill().await;let confirmed=child.wait().await.is_ok();(if tree_stopped&&confirmed{"timed_out"}else{"unknown"},None)},
            };
            #[cfg(windows)]
            drop(tree.take());
            let out = out_task.await;
            let err = err_task.await;
            let final_state = if out.as_ref().map(|v| v.is_ok()).unwrap_or(false)
                && err.as_ref().map(|v| v.is_ok()).unwrap_or(false)
            {
                state
            } else {
                "failed"
            };
            let _ = this.store.job_state(&jid, final_state, code);
            this.running.lock().await.entries.remove(&jid);
        });
        Ok(json!({"jobId":job.id,"state":"running","expiresAt":job.expires_at}))
    }
    pub async fn cancel(&self, client: &str, chat: &str, id: &str) -> Result<Value> {
        let job = self.store.job(id, client, chat)?;
        if let Some((_, sender)) = self.running.lock().await.entries.get_mut(id) {
            if let Some(sender) = sender.take() {
                self.store.job_state(id, "cancel_requested", None)?;
                let _ = sender.send(());
            }
        }
        Ok(json!({"jobId":job.id,"state":self.store.job(id,client,chat)?.state}))
    }
    fn stop_locked(&self, running: &mut Running, client: Option<&str>) {
        for (id, (owner, sender)) in &mut running.entries {
            if client.map(|v| v == owner).unwrap_or(true) {
                if let Some(sender) = sender.take() {
                    let _ = self.store.job_state(id, "cancel_requested", None);
                    let _ = sender.send(());
                }
            }
        }
    }
    pub async fn stop_client(&self, client: Option<&str>) {
        self.stop_locked(&mut *self.running.lock().await, client);
    }
    pub async fn revoke(&self, client: &str) -> Result<()> {
        let mut running = self.running.lock().await;
        self.store.revoke(client)?;
        self.stop_locked(&mut running, Some(client));
        Ok(())
    }
    pub async fn set_enabled(&self, enabled: bool) {
        let mut running = self.running.lock().await;
        running.enabled = enabled;
        if !enabled {
            self.stop_locked(&mut running, None);
        }
    }
    pub async fn status(&self, client: &str, chat: &str, id: &str) -> Result<Value> {
        let j = self.store.job(id, client, chat)?;
        let mut result = serde_json::to_value(&j).unwrap();
        if !matches!(j.state.as_str(), "running" | "queued" | "cancel_requested") {
            for stream in ["stdout", "stderr"] {
                let p = self.root.join("jobs").join(id).join(stream);
                let bytes = match tokio::fs::read(&p).await {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(e) => {
                        result[stream] = json!({"available":false,"error":e.to_string()});
                        continue;
                    }
                };
                result[stream] = json!({"bytes":bytes.len(),"artifact":self.artifacts.put_until(client,chat,&bytes,"application/octet-stream",j.expires_at)?});
            }
        }
        Ok(result)
    }
    pub async fn output(
        &self,
        client: &str,
        chat: &str,
        id: &str,
        stream: &str,
    ) -> Result<Vec<u8>> {
        self.store.job(id, client, chat)?;
        if !matches!(stream, "stdout" | "stderr") {
            return Err("invalid stream".into());
        }
        match tokio::fs::read(self.root.join("jobs").join(id).join(stream)).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn queued_launch_cannot_cross_revocation_barrier() {
        let (root, app, client, call) = crate::lifecycle_tests::fixture().await;
        let barrier = app.jobs.running.lock().await;
        let a = app.clone();
        let c = client.id.clone();
        let revoked = tokio::spawn(async move { a.jobs.revoke(&c).await });
        tokio::task::yield_now().await;
        let a = app.clone();
        let launched = tokio::spawn(async move { a.jobs.launch(&client, &call).await });
        tokio::task::yield_now().await;
        drop(barrier);
        revoked.await.unwrap().unwrap();
        assert_eq!(launched.await.unwrap().unwrap_err(), "UNAUTHORIZED");
        assert!(app.store.jobs().is_empty());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
async fn capture(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    mut f: tokio::fs::File,
) -> Result<()> {
    let mut buf = [0; 16384];
    loop {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
    }
    f.sync_all().await.map_err(|e| e.to_string())
}
