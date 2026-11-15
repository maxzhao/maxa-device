use crate::{model::*, store::Store};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Mutex};
struct Pending {
    public: PairRequest,
    ticket_hash: String,
    credential: Option<Value>,
    rejected: bool,
}
pub struct Pairing {
    identity: Identity,
    pending: Mutex<HashMap<String, Pending>>,
}
// Only pending transport state expires. Approved credentials have no access lifetime.
const PENDING_RETENTION: u64 = 10 * 60 * 1000;
impl Pairing {
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            pending: Mutex::new(HashMap::new()),
        }
    }
    pub fn cleanup(&self) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, p| p.public.created_at + PENDING_RETENTION > now());
    }
    pub fn requests(&self) -> Vec<PairRequest> {
        self.cleanup();
        self.pending
            .lock()
            .unwrap()
            .values()
            .filter(|p| !p.rejected && p.credential.is_none())
            .map(|p| p.public.clone())
            .collect()
    }
    pub fn request(&self, args: &Value) -> Result<Value> {
        self.cleanup();
        if args["protocolVersion"] != 2
            || args["hostId"] != self.identity.host_id
            || args["publicKeyPin"] != self.identity.public_key_pin
        {
            return Err("PAIR_IDENTITY_MISMATCH".into());
        }
        let name = args["clientName"]
            .as_str()
            .filter(|v| !v.trim().is_empty() && v.len() <= 128)
            .ok_or("clientName required (max 128 bytes)")?;
        let client = args["clientIdentity"]
            .as_str()
            .filter(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or("clientIdentity must be 32-byte hex")?;
        let nonce = args["clientNonce"]
            .as_str()
            .filter(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or("clientNonce must be 32-byte hex")?;
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= 16 {
            return Err("PAIR_QUEUE_FULL: complete or cancel an existing local request".into());
        }
        let request = id();
        let server_nonce = format!("{}{}", id().replace('-', ""), id().replace('-', ""));
        let ticket = format!("{}{}", id(), id());
        // Every field is newline-free UUID/hex/SPKI; domain and both nonces bind this exact request.
        let transcript = [
            "maxa-device-pair-v2",
            self.identity.host_id.as_str(),
            self.identity.public_key_pin.as_str(),
            client,
            nonce,
            request.as_str(),
            server_nonce.as_str(),
        ]
        .join("\n");
        let proof = digest(transcript.as_bytes());
        let verification = format!("{}-{}-{}", &proof[..4], &proof[4..8], &proof[8..12]);
        let created = now();
        pending.insert(
            request.clone(),
            Pending {
                public: PairRequest {
                    id: request.clone(),
                    client_name: name.into(),
                    client_identity: client.into(),
                    verification_code: verification.clone(),
                    created_at: created,
                },
                ticket_hash: digest(ticket.as_bytes()),
                credential: None,
                rejected: false,
            },
        );
        Ok(
            json!({"requestId":request,"ticket":ticket,"verificationCode":verification,"serverNonce":server_nonce,"createdAt":created,"state":"pending"}),
        )
    }
    pub fn decide(&self, request: &str, accept: bool, store: &Store) -> Result<()> {
        self.cleanup();
        let mut pending = self.pending.lock().unwrap();
        let p = pending
            .get_mut(request)
            .ok_or("pair request not found or cleared")?;
        if p.rejected || p.credential.is_some() {
            return Err("pair request already decided".into());
        }
        if !accept {
            p.rejected = true;
            return Ok(());
        }
        let token = format!("{}{}", id(), id());
        let client = store.add_client(&p.public.client_name, &token)?;
        p.credential = Some(
            json!({"protocolVersion":2,"hostId":self.identity.host_id,"clientId":client,"token":token,"publicKeyPin":self.identity.public_key_pin}),
        );
        Ok(())
    }
    pub fn poll(&self, args: &Value) -> Result<Value> {
        self.cleanup();
        let pending = self.pending.lock().unwrap();
        let p = Self::select(&pending, args)?;
        if p.rejected {
            return Err("pair request rejected or cancelled".into());
        }
        Ok(p.credential.clone().unwrap_or(json!({"state":"pending"})))
    }
    pub fn cancel(&self, args: &Value) -> Result<Option<String>> {
        let mut pending = self.pending.lock().unwrap();
        let p = Self::select(&pending, args)?;
        let client = p
            .credential
            .as_ref()
            .and_then(|c| c["clientId"].as_str())
            .map(str::to_owned);
        let id = p.public.id.clone();
        pending.remove(&id);
        Ok(client)
    }
    fn select<'a>(pending: &'a HashMap<String, Pending>, args: &Value) -> Result<&'a Pending> {
        let request = args["requestId"].as_str().ok_or("requestId required")?;
        let ticket = args["ticket"].as_str().ok_or("ticket required")?;
        let p = pending
            .get(request)
            .ok_or("pair request not found or cleared")?;
        if p.ticket_hash != digest(ticket.as_bytes()) {
            return Err("invalid pair ticket".into());
        }
        Ok(p)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_pairing_binds_identity_and_requires_local_decision() {
        let dir = std::env::temp_dir().join(id());
        std::fs::create_dir(&dir).unwrap();
        let store = Store::open(&dir.join("state.sqlite")).unwrap();
        let identity = Identity {
            host_id: id(),
            display_name: "test".into(),
            public_key_pin: "sha256//public-test".into(),
        };
        let p = Pairing::new(identity.clone());
        let args = json!({"protocolVersion":2,"hostId":identity.host_id,"publicKeyPin":identity.public_key_pin,"clientName":"client","clientIdentity":"a".repeat(64),"clientNonce":"b".repeat(64)});
        let request = p.request(&args).unwrap();
        let auth = json!({"requestId":request["requestId"],"ticket":request["ticket"]});
        assert_eq!(p.poll(&auth).unwrap()["state"], "pending");
        assert!(store.clients().is_empty());
        let mut changed = args.clone();
        changed["publicKeyPin"] = json!("other");
        assert!(p.request(&changed).is_err());
        p.decide(request["requestId"].as_str().unwrap(), true, &store)
            .unwrap();
        let approved = p.poll(&auth).unwrap();
        assert!(store
            .authenticate(approved["token"].as_str().unwrap())
            .is_ok());
        let second = p.request(&args).unwrap();
        assert_ne!(request["verificationCode"], second["verificationCode"]);
        p.decide(second["requestId"].as_str().unwrap(), false, &store)
            .unwrap();
        assert!(p
            .poll(&json!({"requestId":second["requestId"],"ticket":second["ticket"]}))
            .is_err());
        let client = p.cancel(&auth).unwrap().unwrap();
        store.revoke(&client).unwrap();
        assert!(store
            .authenticate(approved["token"].as_str().unwrap())
            .is_err());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
