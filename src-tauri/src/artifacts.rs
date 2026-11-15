use crate::{identity::private_write, model::*};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

// Only App-owned immutable copies are downloadable. User paths are never object identities.
pub struct Artifacts {
    root: PathBuf,
    host: String,
}
impl Artifacts {
    pub fn new(data: &Path, host: &str) -> Self {
        Self {
            root: data.join("artifacts"),
            host: host.into(),
        }
    }
    pub fn put(&self, client: &str, chat: &str, bytes: &[u8], mime: &str) -> Result<Value> {
        self.put_until(client, chat, bytes, mime, now() + 7 * 86400 * 1000)
    }
    pub fn put_until(
        &self,
        client: &str,
        chat: &str,
        bytes: &[u8],
        mime: &str,
        expires: u64,
    ) -> Result<Value> {
        fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let object = id();
        let reference = json!({"hostId":self.host,"clientId":client,"chatId":chat,"artifactId":object,"sha256":digest(bytes),"bytes":bytes.len(),"mimeType":mime,"expiresAt":expires});
        private_write(&self.root.join(&object), bytes)?;
        if let Err(e) = private_write(
            &self.root.join(format!("{object}.json")),
            reference.to_string().as_bytes(),
        ) {
            let _ = fs::remove_file(self.root.join(&object));
            return Err(e);
        }
        Ok(reference)
    }
    pub fn get(&self, client: &str, chat: &str, reference: &Value) -> Result<Vec<u8>> {
        let object = reference["artifactId"]
            .as_str()
            .ok_or("artifactId required")?;
        if uuid::Uuid::parse_str(object).is_err() {
            return Err("UNKNOWN_ARTIFACT".into());
        }
        let saved: Value = serde_json::from_slice(
            &fs::read(self.root.join(format!("{object}.json"))).map_err(|_| "UNKNOWN_ARTIFACT")?,
        )
        .map_err(|_| "invalid artifact metadata")?;
        if &saved != reference
            || saved["hostId"] != self.host
            || saved["clientId"] != client
            || saved["chatId"] != chat
        {
            return Err("PERMISSION_DENIED: artifact owner/version".into());
        }
        if saved["expiresAt"].as_u64().unwrap_or(0) <= now() {
            return Err("EXPIRED: artifact".into());
        }
        let bytes = fs::read(self.root.join(object)).map_err(|e| e.to_string())?;
        if saved["sha256"] != digest(&bytes) || saved["bytes"].as_u64() != Some(bytes.len() as u64)
        {
            return Err("ARTIFACT_INTEGRITY_FAILURE".into());
        }
        Ok(bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_version_and_expiry() {
        let root = std::env::temp_dir().join(id());
        let a = Artifacts::new(&root, "host");
        let r = a
            .put("client", "chat", b"\xff\0raw", "application/octet-stream")
            .unwrap();
        assert_eq!(a.get("client", "chat", &r).unwrap(), b"\xff\0raw");
        assert!(a.get("other", "chat", &r).is_err());
        assert!(a.get("client", "other", &r).is_err());
        let mut changed = r.clone();
        changed["sha256"] = json!("other");
        assert!(a.get("client", "chat", &changed).is_err());
        let mut expired = r.clone();
        expired["expiresAt"] = json!(0);
        private_write(
            &a.root
                .join(format!("{}.json", r["artifactId"].as_str().unwrap())),
            expired.to_string().as_bytes(),
        )
        .unwrap();
        assert!(a
            .get("client", "chat", &expired)
            .unwrap_err()
            .starts_with("EXPIRED"));
        fs::remove_dir_all(root).unwrap();
    }
}
