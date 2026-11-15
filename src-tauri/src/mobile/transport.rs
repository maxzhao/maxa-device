use crate::model::{digest, Result};
use adbutils::{proto::Network, AdbClient, AdbDevice};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

pub const REMOTE_JAR: &str = "/data/local/tmp/maxa-device-u2-0.4.0-maxa.3.jar";
pub const MAIN_CLASS: &str = "com.wetest.uia2.Main";
pub const PORT: u16 = 9008;
pub const JAR_SHA256: &str = "fac36217adbf518cbaf217b22e39e62f2c7cbbf988d8b5017f2ed75a3445c2ae";
const IO_TIMEOUT: Duration = Duration::from_secs(15);
pub fn client(port: u16) -> AdbClient {
    AdbClient::new("127.0.0.1", port, Some(IO_TIMEOUT))
}
pub async fn shell(dev: &AdbDevice, command: String) -> Result<String> {
    let r = tokio::time::timeout(IO_TIMEOUT, dev.shell2(command, true))
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: Android shell timeout; do not replay")?
        .map_err(|e| format!("OUTCOME_UNKNOWN: Android shell: {e}"))?;
    if r.returncode != 0 {
        return Err(format!(
            "ANDROID_COMMAND_FAILED ({}): {}",
            r.returncode,
            r.output.trim()
        ));
    }
    Ok(r.output.trim_end().into())
}
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
pub fn cmd(args: &[&str]) -> String {
    args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
}
// HTTP is carried by an ADB device tcp stream: no host listener or forward is created.
pub async fn http(
    dev: &AdbDevice,
    method: &str,
    path: &str,
    data: Option<&Value>,
) -> Result<Vec<u8>> {
    let operation = async {
        let mut socket = dev
            .create_connection(Network::Tcp, &PORT.to_string())
            .await
            .map_err(|e| e.to_string())?;
        let body = data
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let header=format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept-Encoding: \r\nContent-Type: application/json; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",body.len());
        socket
            .write_all(header.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        socket.write_all(&body).await.map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        socket
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        if raw.len() > 32 * 1024 * 1024 {
            return Err("helper response exceeds 32 MiB".into());
        }
        parse_http(&raw)
    };
    tokio::time::timeout(IO_TIMEOUT, operation)
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: helper timeout; no retry or action replay")?
        .map_err(|e| format!("OUTCOME_UNKNOWN: helper disconnected/HTTP failure: {e}; no retry"))
}
fn parse_http(raw: &[u8]) -> Result<Vec<u8>> {
    let n = raw
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .ok_or("invalid helper HTTP headers")?;
    let head = std::str::from_utf8(&raw[..n]).map_err(|e| e.to_string())?;
    if head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        != Some("200")
    {
        return Err("helper returned non-200 status".into());
    }
    let body = &raw[n + 4..];
    if head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:"))
    {
        return Err("unsupported helper transfer encoding".into());
    }
    let length = head.lines().find_map(|l| {
        l.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim().parse::<usize>())
    });
    if let Some(length) = length {
        if length.map_err(|_| "invalid content-length")? != body.len() {
            return Err("truncated helper response".into());
        }
    }
    Ok(body.to_vec())
}
pub async fn rpc(dev: &AdbDevice, method: &str, params: Value) -> Result<Value> {
    let id = crate::model::id();
    let body = http(
        dev,
        "POST",
        "/jsonrpc/0",
        Some(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})),
    )
    .await?;
    let v: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("OUTCOME_UNKNOWN: invalid helper JSON: {e}"))?;
    if v["id"] != id {
        return Err("OUTCOME_UNKNOWN: helper response id mismatch".into());
    }
    if let Some(e) = v.get("error").filter(|v| !v.is_null()) {
        let detail = e.to_string();
        if [
            "UiAutomation not connected",
            "DeadObjectException",
            "DeadSystemRuntimeException",
        ]
        .iter()
        .any(|s| detail.contains(s))
        {
            return Err(format!(
                "OUTCOME_UNKNOWN: helper automation disconnected: {detail}"
            ));
        }
        return Err(format!("ANDROID_HELPER_ERROR: {e}"));
    }
    v.get("result")
        .cloned()
        .ok_or("OUTCOME_UNKNOWN: missing helper result".into())
}
pub async fn action(dev: &AdbDevice, method: &str, params: Value) -> Result<()> {
    if rpc(dev, method, params).await? == false {
        return Err(format!(
            "UNSUPPORTED: helper {method} rejected input; inspect Android input permission"
        ));
    }
    Ok(())
}

pub struct OwnedProcess {
    pub pid: u32,
    pub cmdline: String,
    birth: String,
    pub done: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
impl OwnedProcess {
    pub async fn launch(
        dev: &AdbDevice,
        command: &str,
        expected: &str,
        failed_output: Option<&str>,
    ) -> Result<Self> {
        // Persist launch identity before exec: even a lost/short stream response can
        // be reconciled without adopting a process found by name or replaying launch.
        let receipt = format!("/data/local/tmp/maxa-launch-{}.pid", crate::model::id());
        let script = format!(
            "umask 077; echo $$ > {receipt} && cat /proc/$$/stat >> {receipt} && echo MAXA_PID=$$ && exec {command}"
        );
        let attempt = async {
            let mut stream = dev.shell_stream(script).await.map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let pid = loop {
                let b = tokio::time::timeout_at(deadline, stream.read_some(1024))
                    .await
                    .map_err(|_| "owned process PID timed out")?
                    .map_err(|e| e.to_string())?;
                if b.is_empty() || bytes.len() > 8192 {
                    return Err("owned process did not publish PID".into());
                }
                bytes.extend(b);
                // Do not accept a split, incomplete numeric marker.
                if let Some(pid) = bytes
                    .split_inclusive(|b| *b == b'\n')
                    .filter(|l| l.ends_with(b"\n"))
                    .find_map(|l| {
                        std::str::from_utf8(l)
                            .ok()?
                            .trim()
                            .strip_prefix("MAXA_PID=")?
                            .parse::<u32>()
                            .ok()
                    })
                {
                    break pid;
                }
            };
            let saved = shell(dev, format!("cat {receipt}")).await?;
            let (saved_pid, saved_birth) = launch_identity(&saved)?;
            if pid != saved_pid {
                return Err("launch PID receipt mismatch".into());
            }
            // The marker precedes exec. Confirm the target argv, not the shell argv.
            let identity_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let cmdline = loop {
                let c = read_cmdline(dev, pid).await?;
                if expected_process(&c, expected) && birth(dev, pid).await? == saved_birth {
                    break c;
                }
                if tokio::time::Instant::now() >= identity_deadline {
                    return Err("owned process did not confirm exec identity".into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            let done = Arc::new(AtomicBool::new(false));
            let ended = done.clone();
            stream.set_timeout(None);
            let reader = tokio::spawn(async move {
                while let Ok(b) = stream.read_some(4096).await {
                    if b.is_empty() {
                        break;
                    }
                }
                ended.store(true, Ordering::SeqCst);
            });
            Ok::<_, String>(Self {
                pid,
                cmdline,
                birth: saved_birth,
                done,
                reader,
            })
        }
        .await;
        match attempt {
            Ok(process) => {
                let _ = shell(dev, cmd(&["rm", "-f", &receipt])).await;
                Ok(process)
            }
            Err(e) => {
                let cleanup = cleanup_launch(dev, &receipt, expected).await;
                Err(match cleanup {
                    Ok(()) => {
                        if let Some(path) = failed_output {
                            if let Err(c) = shell(dev, cmd(&["rm", "-f", path])).await {
                                return Err(format!("OUTCOME_UNKNOWN: {e}; owned launch stopped; output cleanup failed ({path}): {c}; do not replay"));
                            }
                        }
                        format!("OUTCOME_UNKNOWN: {e}; owned launch cleaned up; do not replay")
                    }
                    Err(c) => format!("OUTCOME_UNKNOWN: {e}; launch cleanup unconfirmed ({receipt}): {c}; do not replay"),
                })
            }
        }
    }
    pub async fn matches(&self, dev: &AdbDevice) -> bool {
        matches!((read_cmdline(dev,self.pid).await,birth(dev,self.pid).await),(Ok(c),Ok(b)) if c==self.cmdline && b==self.birth)
    }
    pub async fn signal(&self, dev: &AdbDevice, signal: u8) -> Result<()> {
        if self.gone(dev).await? {
            return Ok(());
        }
        if !self.matches(dev).await {
            return Err(
                "ANDROID_OWNERSHIP_UNKNOWN: PID identity changed; refusing to signal".into(),
            );
        }
        shell(dev, format!("kill -{signal} {}", self.pid)).await?;
        Ok(())
    }
    pub async fn gone(&self, dev: &AdbDevice) -> Result<bool> {
        let out = shell(
            dev,
            format!(
                "if [ -d /proc/{} ]; then echo PRESENT; else echo GONE; fi",
                self.pid
            ),
        )
        .await?;
        match out.trim() {
            "GONE" => Ok(true),
            "PRESENT" => Ok(false),
            _ => Err("cannot inspect owned PID liveness".into()),
        }
    }
    pub async fn terminate(&self, dev: &AdbDevice, signal: u8) -> Result<()> {
        self.signal(dev, signal).await?;
        for _ in 0..100 {
            if self.gone(dev).await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("OUTCOME_UNKNOWN: owned PID termination not confirmed".into())
    }
}
fn expected_process(cmdline: &str, expected: &str) -> bool {
    let executable = cmdline
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    (executable == expected || cmdline.split_whitespace().any(|s| s == expected))
        && (executable.starts_with("app_process") || executable == expected)
}
fn stat_birth(stat: &str) -> Result<String> {
    stat.rsplit_once(") ")
        .and_then(|(_, s)| s.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or("cannot read process birth identity".into())
}
fn launch_identity(receipt: &str) -> Result<(u32, String)> {
    let (pid, stat) = receipt.split_once('\n').ok_or("invalid launch receipt")?;
    let pid = pid
        .trim()
        .parse::<u32>()
        .map_err(|_| "invalid launch PID")?;
    if pid <= 1 || !stat.starts_with(&format!("{pid} (")) {
        return Err("invalid launch identity".into());
    }
    Ok((pid, stat_birth(stat)?))
}
async fn cleanup_launch(dev: &AdbDevice, receipt: &str, expected: &str) -> Result<()> {
    let saved = shell(
        dev,
        format!("if [ -f {receipt} ]; then cat {receipt}; else echo ABSENT; fi"),
    )
    .await?;
    if saved.trim() == "ABSENT" {
        // No receipt is not evidence that a disconnected launch never reached the phone.
        return Err("launch receipt unavailable; no unowned process signalled".into());
    }
    let (pid, saved_birth) = launch_identity(&saved)?;
    let present = shell(
        dev,
        format!("if [ -d /proc/{pid} ]; then echo PRESENT; else echo GONE; fi"),
    )
    .await?;
    if present.trim() == "GONE" {
        shell(dev, cmd(&["rm", "-f", receipt])).await?;
        return Ok(());
    }
    let c = read_cmdline(dev, pid).await?;
    if birth(dev, pid).await? != saved_birth {
        return Err("PID was reused; no process signalled".into());
    }
    // The wrapper may not have exec'd yet. Its unique receipt must be in argv.
    let shell_wrapper = c.split_whitespace().next().unwrap_or("").rsplit('/').next() == Some("sh")
        && c.contains(receipt);
    if !expected_process(&c, expected) && !shell_wrapper {
        return Err("launch command identity changed; no process signalled".into());
    }
    shell(dev, format!("kill -9 {pid}")).await?;
    // Only one signal, never relaunch or reissue an action.
    for _ in 0..100 {
        let gone = shell(
            dev,
            format!("if [ -d /proc/{pid} ]; then echo PRESENT; else echo GONE; fi"),
        )
        .await?;
        if gone.trim() == "GONE" {
            shell(dev, cmd(&["rm", "-f", receipt])).await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    return Err("owned launch termination not confirmed".into());
}
async fn birth(dev: &AdbDevice, pid: u32) -> Result<String> {
    let s = shell(dev, format!("cat /proc/{pid}/stat")).await?;
    stat_birth(&s)
}
pub async fn read_cmdline(dev: &AdbDevice, pid: u32) -> Result<String> {
    Ok(shell(dev, format!("cat /proc/{pid}/cmdline"))
        .await?
        .replace('\0', " ")
        .trim()
        .into())
}
pub async fn launch_helper(
    dev: &AdbDevice,
    jar: &Path,
    cancelled: &AtomicBool,
) -> Result<OwnedProcess> {
    let bytes = tokio::fs::read(jar)
        .await
        .map_err(|e| format!("phone helper pack missing: {e}"))?;
    if digest(&bytes) != JAR_SHA256 {
        return Err("phone helper SHA256 mismatch".into());
    }
    if cancelled.load(Ordering::SeqCst) {
        return Err("CONTROL_STOPPED before helper push".into());
    }
    dev.sync()
        .push_bytes(&bytes, REMOTE_JAR, 0o600)
        .await
        .map_err(|e| e.to_string())?;
    let process = OwnedProcess::launch(
        dev,
        &format!("env CLASSPATH={REMOTE_JAR} app_process / {MAIN_CLASS} -p {PORT}"),
        MAIN_CLASS,
        None,
    )
    .await?;
    let ready = async {
        for _ in 0..60 {
            if cancelled.load(Ordering::SeqCst) {
                return Err("CONTROL_STOPPED during helper startup".into());
            }
            if process.done.load(Ordering::SeqCst) {
                return Err(
                    "phone helper exited; inspect UiAutomation ownership/OS permission".into(),
                );
            }
            if let Ok(body) = http(dev, "GET", "/ping", None).await {
                if body == b"pong" {
                    let tcp = shell(dev, "cat /proc/net/tcp /proc/net/tcp6".into())
                        .await
                        .map_err(|e| {
                            format!("PHONE_HELPER_EXPOSED: cannot verify phone listener: {e}")
                        })?;
                    verify_loopback(&tcp, PORT)?;
                    if !process.matches(dev).await {
                        return Err("helper PID ownership changed".into());
                    }
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err("phone helper startup timeout".into())
    }
    .await;
    if let Err(e) = ready {
        let stopped = process.terminate(dev, 9).await;
        return Err(match stopped {
            Ok(_) => e,
            Err(stop) => format!("{e}; owned helper cleanup failed: {stop}"),
        });
    }
    Ok(process)
}
// /proc addresses are native little-endian words, including each IPv6 u32.
pub fn verify_loopback(tcp: &str, port: u16) -> Result<()> {
    let mut found = false;
    for line in tcp.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.len() < 4 || f[3] != "0A" {
            continue;
        }
        let Some((address, p)) = f[1].split_once(':') else {
            continue;
        };
        if u16::from_str_radix(p, 16).ok() != Some(port) {
            continue;
        }
        found = true;
        let loopback = match address.len() {
            8 => u32::from_str_radix(address, 16)
                .map(|v| v.to_le_bytes()[0] == 127)
                .unwrap_or(false),
            32 => {
                address.eq_ignore_ascii_case("00000000000000000000000001000000")
                    || (address[..24].eq_ignore_ascii_case("0000000000000000FFFF0000")
                        && u32::from_str_radix(&address[24..], 16)
                            .map(|v| v.to_le_bytes()[0] == 127)
                            .unwrap_or(false))
            }
            _ => false,
        };
        if !loopback {
            return Err("PHONE_HELPER_EXPOSED: non-loopback phone listener; helper terminated, control refused".into());
        }
    }
    if !found {
        return Err(
            "PHONE_HELPER_EXPOSED: cannot verify loopback phone listener; control refused".into(),
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn listeners_fail_closed_for_wildcard_external_and_missing() {
        let row = |a: &str| format!("0: {a}:2330 00000000:0000 0A 0");
        assert!(verify_loopback(&row("0100007F"), 9008).is_ok());
        assert!(verify_loopback(&row("0000000000000000FFFF00000100007F"), 9008).is_ok());
        assert!(verify_loopback(&row("0000000000000000FFFF00000101A8C0"), 9008).is_err());
        assert!(verify_loopback(&row("00000000000000000000000001000000"), 9008).is_ok());
        for a in ["00000000", "00000000000000000000000000000000", "0101A8C0"] {
            assert!(verify_loopback(&row(a), 9008).is_err());
        }
        assert!(verify_loopback("permission denied", 9008).is_err());
        assert!(verify_loopback(&(row("0100007F") + "\n" + &row("00000000")), 9008).is_err());
    }
    #[test]
    fn http_rejects_truncation_and_shell_arguments_are_quoted() {
        assert_eq!(
            parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong").unwrap(),
            b"pong"
        );
        assert!(parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npong").is_err());
        assert_eq!(
            quote("a'; reboot; '"),
            "'a'\\''; reboot; '\\''' ".trim_end()
        );
    }
}
