//! Frozen loopback companion v1. Authenticate the server before disclosing any call body.
use crate::model::{digest, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{collections::HashMap, path::Path, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};
const LIMIT: usize = 1_048_576;
#[derive(Clone, Serialize, Deserialize)]
pub struct Link {
    pub port: u16,
    pub secret: String,
    pub version: String,
}
impl Link {
    pub fn validate(&self) -> Result<Vec<u8>> {
        let key = URL_SAFE_NO_PAD
            .decode(&self.secret)
            .map_err(|_| "INVALID_ARGUMENT: invalid link secret")?;
        if self.port == 0 || key.len() != 32 || self.secret.len() != 43 {
            return Err("INVALID_ARGUMENT: invalid link".into());
        }
        Ok(key)
    }
    pub fn load(root: &Path) -> Result<Self> {
        let link: Self = serde_json::from_slice(
            &std::fs::read(root.join("companion.json"))
                .map_err(|_| "Companion not linked; use maxa-device link")?,
        )
        .map_err(|_| "Invalid companion link file")?;
        link.validate()?;
        Ok(link)
    }
}
struct Session {
    id: String,
    key: Vec<u8>,
    seq: u64,
}
pub struct Client {
    link: Link,
    session: Mutex<Option<Session>>,
}
pub struct Reply {
    pub bytes: Vec<u8>,
    pub meta: Option<Value>,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn mac(key: &[u8], parts: &[&str]) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    for (i, part) in parts.iter().enumerate() {
        if i != 0 {
            h.update(&[0]);
        }
        h.update(part.as_bytes());
    }
    h.finalize().into_bytes().to_vec()
}
fn verify(expected: &[u8], supplied: &str) -> Result<()> {
    if hex(expected)
        .as_bytes()
        .ct_eq(supplied.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err("AUTH_FAILED: companion proof/MAC rejected".into());
    }
    Ok(())
}
fn nonce() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| "Random generator unavailable")?;
    Ok(hex(&bytes))
}
fn nonce_valid(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
struct HttpReply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
async fn http(port: u16, path: &str, headers: &[(&str, String)], body: &[u8]) -> Result<HttpReply> {
    if body.len() > LIMIT {
        return Err("INVALID_ARGUMENT: companion body exceeds 1 MiB".into());
    }
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: companion disconnected; no replay")?;
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",body.len());
    for (k, v) in headers {
        if v.contains(['\r', '\n']) {
            return Err("Invalid companion header".into());
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: companion write failed; no replay")?;
    stream
        .write_all(body)
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: companion write failed; no replay")?;
    let mut bytes = Vec::new();
    let end;
    loop {
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|_| "OUTCOME_UNKNOWN: incomplete companion response")?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            end = bytes.len();
            break;
        }
        if bytes.len() > 16384 {
            return Err("Invalid companion response headers".into());
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid companion HTTP")?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("Invalid companion HTTP status")?;
    let mut fields = HashMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':').ok_or("Invalid companion header")?;
        if fields
            .insert(k.to_ascii_lowercase(), v.trim().to_string())
            .is_some()
        {
            return Err("Duplicate companion header".into());
        }
    }
    if fields.contains_key("transfer-encoding") {
        return Err("Companion chunked encoding unsupported".into());
    }
    let len: usize = if status == 204 {
        0
    } else {
        fields
            .get("content-length")
            .ok_or("Companion Content-Length missing")?
            .parse()
            .map_err(|_| "Invalid companion Content-Length")?
    };
    if len > LIMIT {
        return Err("Companion response exceeds 1 MiB".into());
    }
    bytes.resize(end + len, 0);
    stream
        .read_exact(&mut bytes[end..])
        .await
        .map_err(|_| "OUTCOME_UNKNOWN: incomplete companion body")?;
    Ok(HttpReply {
        status,
        headers: fields,
        body: bytes[end..].to_vec(),
    })
}
impl Client {
    pub fn new(link: Link) -> Result<Self> {
        link.validate()?;
        Ok(Self {
            link,
            session: Mutex::new(None),
        })
    }
    async fn authenticate(&self) -> Result<Session> {
        let key = self.link.validate()?;
        let client = nonce()?;
        let response = http(
            self.link.port,
            "/v1/session",
            &[],
            &serde_json::to_vec(&json!({"clientNonce":client})).unwrap(),
        )
        .await?;
        if response.status != 200 {
            return Err("AUTH_FAILED: companion challenge rejected".into());
        }
        let v: Value = serde_json::from_slice(&response.body)
            .map_err(|_| "AUTH_FAILED: invalid companion challenge")?;
        let server = v["serverNonce"]
            .as_str()
            .filter(|s| nonce_valid(s, 64))
            .ok_or("AUTH_FAILED: invalid server nonce")?;
        let id = v["sessionId"]
            .as_str()
            .filter(|s| nonce_valid(s, 32))
            .ok_or("AUTH_FAILED: invalid session id")?;
        verify(
            &mac(&key, &["server", &client, server]),
            v["serverProof"].as_str().unwrap_or(""),
        )?;
        let proof = hex(&mac(&key, &["client", server, &client]));
        let response = http(
            self.link.port,
            "/v1/session/confirm",
            &[],
            &serde_json::to_vec(&json!({"sessionId":id,"clientProof":proof})).unwrap(),
        )
        .await?;
        if response.status != 204 {
            return Err("AUTH_FAILED: companion confirmation rejected".into());
        }
        Ok(Session {
            id: id.into(),
            key: mac(&key, &["session", &client, server]),
            seq: 0,
        })
    }
    pub async fn raw(&self, body: &[u8], meta: Option<&str>) -> Result<Reply> {
        let mut slot = self.session.lock().await;
        for attempt in 0..2 {
            if slot.is_none() {
                *slot = Some(
                    tokio::time::timeout(Duration::from_secs(5), self.authenticate())
                        .await
                        .map_err(|_| "AUTH_FAILED: companion handshake timed out")??,
                );
            }
            let session = slot.as_mut().unwrap();
            session.seq = session.seq.checked_add(1).ok_or("Sequence exhausted")?;
            let seq = session.seq.to_string();
            let hash = digest(body);
            let mut parts = vec!["req", &seq, &hash];
            if let Some(m) = meta {
                parts.push(m);
            }
            let mut headers = vec![
                ("X-Maxa-Session", session.id.clone()),
                ("X-Maxa-Seq", seq.clone()),
                ("X-Maxa-Mac", hex(&mac(&session.key, &parts))),
            ];
            if let Some(m) = meta {
                headers.push(("X-Maxa-Meta", m.into()));
            }
            let response = tokio::time::timeout(
                Duration::from_secs(190),
                http(self.link.port, "/v1/call", &headers, body),
            )
            .await
            .map_err(|_| "OUTCOME_UNKNOWN: companion call timed out; no replay".to_string())
            .and_then(|v| v);
            let result = match response {
                Ok(r) => {
                    if r.status == 401 {
                        slot.take();
                        if attempt == 0 {
                            // The frozen protocol guarantees 401 has no effect. Authenticate
                            // again before resending these same body/meta bytes, exactly once.
                            continue;
                        }
                        return Err("AUTH_FAILED: companion session rejected; no replay".into());
                    } else {
                        let hash = digest(&r.body);
                        let mut parts = vec!["resp", &seq, &hash];
                        let meta = r.headers.get("x-maxa-meta");
                        if let Some(m) = meta {
                            parts.push(m);
                        }
                        verify(
                            &mac(&session.key, &parts),
                            r.headers
                                .get("x-maxa-mac")
                                .map(String::as_str)
                                .unwrap_or(""),
                        )
                        .and_then(|_| {
                            let meta = meta
                                .map(|m| {
                                    serde_json::from_str(m).map_err(|_| {
                                        "Invalid authenticated companion metadata".to_string()
                                    })
                                })
                                .transpose()?;
                            Ok(Reply {
                                bytes: r.body,
                                meta,
                            })
                        })
                    }
                }
                Err(e) => Err(e),
            };
            if result.is_err() {
                slot.take();
            } // Transport errors, bad MACs and all other statuses are never resent.
            return result;
        }
        unreachable!("two attempts always return")
    }
    pub async fn call(&self, op: &str, args: Value) -> Result<Value> {
        self.call_id(&crate::model::id(), op, args).await
    }
    pub async fn call_id(&self, request: &str, op: &str, args: Value) -> Result<Value> {
        let r = self
            .raw(
                &serde_json::to_vec(&json!({"requestId":request,"op":op,"args":args})).unwrap(),
                None,
            )
            .await?;
        let v: Value = serde_json::from_slice(&r.bytes)
            .map_err(|_| "OUTCOME_UNKNOWN: invalid authenticated companion result")?;
        result(v)
    }
    pub async fn status(&self) -> Result<Value> {
        let v = tokio::time::timeout(Duration::from_secs(5), self.call("status", json!({})))
            .await
            .map_err(|_| "Companion status timed out")??;
        if v["protocol"] != 1 {
            return Err("Companion protocol mismatch (requires 1)".into());
        }
        if v["version"] != env!("CARGO_PKG_VERSION") {
            return Err(format!(
                "version mismatch: agent {}, companion {}",
                env!("CARGO_PKG_VERSION"),
                v["version"].as_str().unwrap_or("unknown")
            ));
        }
        Ok(v)
    }
    pub async fn cancel(&self, request: &str) -> Result<()> {
        // Separate session so a long in-flight call cannot block its own cancellation.
        let c = Self::new(self.link.clone())?;
        c.call("cancel", json!({"requestId":request})).await?;
        Ok(())
    }
}
pub fn result(v: Value) -> Result<Value> {
    if v["ok"] == true {
        Ok(v["result"].clone())
    } else {
        Err(format!(
            "{}: {}",
            v["error"]["code"].as_str().unwrap_or("INTERNAL"),
            v["error"]["message"]
                .as_str()
                .unwrap_or("Invalid companion error")
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn call_401_reauthenticates_and_resends_same_call_only_once() {
        use tokio::net::TcpListener;
        async fn receive(
            listener: &TcpListener,
        ) -> (TcpStream, String, HashMap<String, String>, Vec<u8>) {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let mut lines = head.split("\r\n");
            let path = lines
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            let headers: HashMap<_, _> = lines
                .filter_map(|s| s.split_once(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let mut body = vec![0; headers["content-length"].parse().unwrap()];
            stream.read_exact(&mut body).await.unwrap();
            (stream, path, headers, body)
        }
        async fn respond(mut stream: TcpStream, status: u16, body: &[u8], proof: Option<String>) {
            let mac = proof
                .map(|v| format!("X-Maxa-Mac: {v}\r\n"))
                .unwrap_or_default();
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n{mac}Connection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        }
        // JSON and binary/meta calls both retain their exact bytes. A second 401
        // and a bad server proof must stop without another effect-bearing call.
        for (with_meta, second_status, bad_proof) in [
            (false, 200, false),
            (true, 200, false),
            (false, 401, false),
            (false, 200, true),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let secret = vec![42; 32];
            let c = Client::new(Link {
                port: listener.local_addr().unwrap().port(),
                secret: URL_SAFE_NO_PAD.encode(&secret),
                version: env!("CARGO_PKG_VERSION").into(),
            })
            .unwrap();
            *c.session.lock().await = Some(Session {
                id: "expired".into(),
                key: vec![0; 32],
                seq: 9,
            });
            let body = if with_meta {
                vec![7; 32]
            } else {
                br#"{"requestId":"same","op":"ui.tap","args":{"x":1,"y":2}}"#.to_vec()
            };
            let meta = with_meta.then(|| {
                r#"{"requestId":"same","op":"blob.write","args":{"blobId":"slot","offset":0}}"#
                    .to_string()
            });
            let expected = body.clone();
            let expected_meta = meta.clone();
            let server = tokio::spawn(async move {
                let (stream, path, headers, bytes) = receive(&listener).await;
                assert_eq!(path, "/v1/call");
                assert_eq!(bytes, expected);
                assert_eq!(headers["x-maxa-seq"], "10");
                assert_eq!(headers.get("x-maxa-meta"), expected_meta.as_ref());
                respond(stream, 401, b"", None).await;
                let (stream, path, _, bytes) = receive(&listener).await;
                assert_eq!(path, "/v1/session");
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                let client = v["clientNonce"].as_str().unwrap();
                let nonce = "11".repeat(32);
                let sid = "22".repeat(16);
                let key = mac(&secret, &["session", client, &nonce]);
                let proof = if bad_proof {
                    "00".repeat(32)
                } else {
                    hex(&mac(&secret, &["server", client, &nonce]))
                };
                respond(
                    stream,
                    200,
                    &serde_json::to_vec(
                        &json!({"sessionId":sid,"serverNonce":nonce,"serverProof":proof}),
                    )
                    .unwrap(),
                    None,
                )
                .await;
                if bad_proof {
                    return;
                }
                let (stream, path, _, bytes) = receive(&listener).await;
                assert_eq!(path, "/v1/session/confirm");
                let confirm: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(confirm["sessionId"], sid);
                assert_eq!(
                    confirm["clientProof"],
                    hex(&mac(&secret, &["client", &nonce, client]))
                );
                respond(stream, 204, b"", None).await;
                let (stream, path, headers, bytes) = receive(&listener).await;
                assert_eq!(path, "/v1/call");
                assert_eq!(bytes, expected);
                assert_eq!(headers.get("x-maxa-meta"), expected_meta.as_ref());
                assert_eq!(headers["x-maxa-session"], sid);
                assert_eq!(headers["x-maxa-seq"], "1");
                let hash = digest(&bytes);
                let mut parts = vec!["req", "1", &hash];
                if let Some(meta) = expected_meta.as_deref() {
                    parts.push(meta);
                }
                verify(&mac(&key, &parts), &headers["x-maxa-mac"]).unwrap();
                let body = br#"{"ok":true,"result":{}}"#;
                let proof = hex(&mac(&key, &["resp", "1", &digest(body)]));
                respond(stream, second_status, body, Some(proof)).await;
            });
            let result =
                tokio::time::timeout(Duration::from_secs(3), c.raw(&body, meta.as_deref()))
                    .await
                    .unwrap();
            if bad_proof {
                assert!(result
                    .err()
                    .unwrap()
                    .starts_with("AUTH_FAILED: companion proof/MAC rejected"));
            } else if second_status == 401 {
                assert!(result
                    .err()
                    .unwrap()
                    .starts_with("AUTH_FAILED: companion session rejected"));
            } else {
                assert_eq!(result.unwrap().bytes, br#"{"ok":true,"result":{}}"#);
            }
            server.await.unwrap();
        }
    }
    #[test]
    fn frozen_protocol_vector() {
        let secret = (1..=32).collect::<Vec<u8>>();
        let client = "00".repeat(32);
        let server = "11".repeat(32);
        let server_proof = hex(&mac(&secret, &["server", &client, &server]));
        let client_proof = hex(&mac(&secret, &["client", &server, &client]));
        let key = mac(&secret, &["session", &client, &server]);
        let request = hex(&mac(&key, &["req", "1", &digest(b"{}")]));
        println!(
            "serverProof={server_proof}\nclientProof={client_proof}\nk={}\nrequestMac={request}",
            hex(&key)
        );
        assert_eq!(
            client_proof,
            "aa35fa3c58af433fb6938b66353e3acd289e3a9b77dae08cef62181a7cddaa8b"
        );
        assert_eq!(
            hex(&key),
            "c889f83a7ebd29933dfbfa27f8e15eeb06b781c306a9889185748e8b7af45737"
        );
        assert_eq!(
            request,
            "191d1c5ec42f717316898641db24e03b5243199cd953acd859a79bfb3c03860f"
        );
        assert_eq!(
            server_proof,
            "3cff9123aaa9d7f3c06a26002bc007e2e4b26d23d74d2438d75f2f2469a69a34"
        );
    }
}
