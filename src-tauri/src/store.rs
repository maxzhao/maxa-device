use crate::model::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{path::Path, sync::Mutex};

pub struct Store(pub Mutex<Connection>);
const RETENTION: u64 = 7 * 86400 * 1000;
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
CREATE TABLE IF NOT EXISTS clients(id TEXT PRIMARY KEY, name TEXT NOT NULL, token_hash TEXT UNIQUE NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS requests(client TEXT NOT NULL, id TEXT NOT NULL, chat TEXT NOT NULL, hash TEXT NOT NULL, state TEXT NOT NULL, result TEXT, expires INTEGER NOT NULL, PRIMARY KEY(client,id));
        CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, client TEXT NOT NULL, chat TEXT NOT NULL, request TEXT NOT NULL, state TEXT NOT NULL, code INTEGER, created INTEGER NOT NULL, expires INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS preferences(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS copy_temps(path TEXT PRIMARY KEY);
        PRAGMA user_version=2;
        UPDATE requests SET state='unknown' WHERE state='running';
        UPDATE jobs SET state='interrupted' WHERE state IN ('queued','running','cancel_requested');").map_err(|e| e.to_string())?;
        let has_grants = {
            let mut q = db
                .prepare("PRAGMA table_info(clients)")
                .map_err(|e| e.to_string())?;
            let names = q
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|e| e.to_string())?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            names.iter().any(|n| n == "grants")
        };
        if has_grants {
            db.execute_batch("ALTER TABLE clients DROP COLUMN grants;")
                .map_err(|e| format!("trusted-client migration: {e}"))?;
        }
        let abandoned = {
            let mut q = db.prepare("SELECT path FROM copy_temps").map_err(|e| e.to_string())?;
            let rows = q.query_map([], |r| r.get::<_, String>(0)).map_err(|e|e.to_string())?;
            rows.collect::<std::result::Result<Vec<_>,_>>().map_err(|e|e.to_string())?
        };
        for path in abandoned {
            let path = std::path::Path::new(&path);
            let owned = path.file_name().and_then(|v|v.to_str()).and_then(|v|v.strip_prefix(".maxa-copy-")).and_then(|v|v.strip_suffix(".tmp")).is_some_and(|v|uuid::Uuid::parse_str(v).is_ok());
            if owned {
                match crate::transfer::remove_temp(path) {
                    Ok(()) => (),
                    Err(e) if e.kind()==std::io::ErrorKind::NotFound => (),
                    Err(_) => continue,
                }
                db.execute("DELETE FROM copy_temps WHERE path=?1",[path.to_string_lossy().as_ref()]).map_err(|e|e.to_string())?;
            }
        }
        Ok(Self(Mutex::new(db)))
    }
    pub fn copy_temp(&self, path: &Path, add: bool) -> Result<()> {
        self.0.lock().unwrap().execute(if add { "INSERT OR IGNORE INTO copy_temps VALUES(?1)" } else { "DELETE FROM copy_temps WHERE path=?1" },[path.to_str().ok_or("NON_UTF8_COPY_PATH")?]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn preferences(&self) -> Result<crate::preferences::Preferences> {
        let text: Option<String> = self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT value FROM preferences WHERE key='app'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())?;
        let value: crate::preferences::Preferences = match text {
            Some(text) => serde_json::from_str(&text)
                .map_err(|e| format!("Invalid saved preferences: {e}"))?,
            None => Default::default(),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn save_preferences(&self, value: &crate::preferences::Preferences) -> Result<()> {
        value.validate()?;
        let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
        self.0.lock().unwrap().execute(
            "INSERT INTO preferences VALUES('app',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [text]
        ).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn add_client(&self, name: &str, token: &str) -> Result<String> {
        let id = id();
        self.0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO clients(id,name,token_hash) VALUES(?1,?2,?3)",
                params![id, name, digest(token.as_bytes())],
            )
            .map_err(|e| e.to_string())?;
        Ok(id)
    }
    pub fn authenticate(&self, token: &str) -> Result<ClientInfo> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT id,name,revoked FROM clients WHERE token_hash=?1 AND revoked=0",
                [digest(token.as_bytes())],
                |r| {
                    Ok(ClientInfo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        revoked: r.get(2)?,
                    })
                },
            )
            .map_err(|_| "UNAUTHORIZED".into())
    }
    pub fn clients(&self) -> Vec<ClientInfo> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT id,name,revoked FROM clients").unwrap();
        q.query_map([], |r| {
            Ok(ClientInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                revoked: r.get(2)?,
            })
        })
        .unwrap()
        .filter_map(|x| x.ok())
        .collect()
    }
    pub fn active(&self, client: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM clients WHERE id=?1 AND revoked=0",
                [client],
                |_| Ok(()),
            )
            .map_err(|_| "UNAUTHORIZED".into())
    }
    pub fn revoke(&self, client: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("UPDATE clients SET revoked=1 WHERE id=?1", [client])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    // Registration is committed before effects. A crash is deliberately outcome unknown, never a replay.
    pub fn claim(&self, client: &str, call: &Invocation) -> Result<Option<Value>> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction().map_err(|e| e.to_string())?;
        let hash = digest(
            serde_json::to_vec(&(
                call.chat_id.clone(),
                call.operation.clone(),
                &call.arguments,
            ))
            .unwrap()
            .as_slice(),
        );
        let prior: Option<(String, String, Option<String>, u64)> = tx
            .query_row(
                "SELECT hash,state,result,expires FROM requests WHERE client=?1 AND id=?2",
                params![client, call.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((h, state, result, expiry)) = prior {
            if h != hash {
                return Err("REQUEST_ID_CONFLICT".into());
            }
            if expiry < now() {
                return Err("EXPIRED: execution tombstone retained; do not replay".into());
            }
            return Ok(Some(
                result
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(json!({"state":state,"requestId":call.request_id})),
            ));
        }
        tx.execute(
            "INSERT INTO requests VALUES(?1,?2,?3,?4,'running',NULL,?5)",
            params![
                client,
                call.request_id,
                call.chat_id,
                hash,
                now() + RETENTION
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(None)
    }
    pub fn complete(&self, client: &str, request: &str, result: &Value) -> Result<()> {
        self.0.lock().unwrap().execute("UPDATE requests SET state='finished', result=?3 WHERE client=?1 AND id=?2 AND state='running'",params![client,request,result.to_string()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn request(&self, client: &str, request: &str, chat: &str) -> Result<Value> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT state,result,expires FROM requests WHERE client=?1 AND id=?2 AND chat=?3",
                params![client, request, chat],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, u64>(2)?,
                    ))
                },
            )
            .map_err(|_| "UNKNOWN_REQUEST".to_string())
            .and_then(|(state, result, expires)| {
                if expires < now() {
                    Err("EXPIRED".into())
                } else {
                    Ok(result
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(json!({"state":state})))
                }
            })
    }
    pub fn new_job(&self, client: &str, call: &Invocation) -> Result<JobInfo> {
        let job = JobInfo {
            id: id(),
            request_id: call.request_id.clone(),
            client_id: client.into(),
            chat_id: call.chat_id.clone(),
            state: "queued".into(),
            exit_code: None,
            created_at: now(),
            expires_at: now() + RETENTION,
        };
        self.0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO jobs VALUES(?1,?2,?3,?4,?5,NULL,?6,?7)",
                params![
                    job.id,
                    client,
                    call.chat_id,
                    call.request_id,
                    job.state,
                    job.created_at,
                    job.expires_at
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(job)
    }
    pub fn job_state(&self, id: &str, state: &str, code: Option<i32>) -> Result<()> {
        self.0.lock().unwrap().execute("UPDATE jobs SET state=?2,code=?3 WHERE id=?1 AND state IN ('queued','running','cancel_requested')",params![id,state,code]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn jobs(&self) -> Vec<JobInfo> {
        let db = self.0.lock().unwrap();
        let mut q=db.prepare("SELECT id,request,client,chat,state,code,created,expires FROM jobs ORDER BY created DESC").unwrap();
        q.query_map([], |r| {
            Ok(JobInfo {
                id: r.get(0)?,
                request_id: r.get(1)?,
                client_id: r.get(2)?,
                chat_id: r.get(3)?,
                state: r.get(4)?,
                exit_code: r.get(5)?,
                created_at: r.get(6)?,
                expires_at: r.get(7)?,
            })
        })
        .unwrap()
        .filter_map(|x| x.ok())
        .collect()
    }
    pub fn job(&self, id: &str, client: &str, chat: &str) -> Result<JobInfo> {
        self.jobs()
            .into_iter()
            .find(|j| j.id == id && j.client_id == client && j.chat_id == chat)
            .ok_or("UNKNOWN_JOB".into())
            .and_then(|j| {
                if j.expires_at < now() {
                    Err("EXPIRED".into())
                } else {
                    Ok(j)
                }
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trusted_migration_preserves_approved_and_revoked_identities() {
        let path = std::env::temp_dir().join(format!("maxa-migration-{}.sqlite", id()));
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE clients(id TEXT PRIMARY KEY,name TEXT NOT NULL,token_hash TEXT UNIQUE NOT NULL,grants TEXT NOT NULL,revoked INTEGER NOT NULL DEFAULT 0)").unwrap();
        db.execute(
            "INSERT INTO clients VALUES('approved','old',?1,'{\"execute\":false}',0)",
            [digest(b"approved-fixture")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO clients VALUES('revoked','old revoked',?1,'{}',1)",
            [digest(b"revoked-fixture")],
        )
        .unwrap();
        drop(db);
        let migrated = Store::open(&path).unwrap();
        let approved = migrated.authenticate("approved-fixture").unwrap();
        assert_eq!(approved.id, "approved");
        assert!(serde_json::to_value(&approved)
            .unwrap()
            .get("grants")
            .is_none());
        assert!(migrated.authenticate("revoked-fixture").is_err());
        assert!(migrated
            .clients()
            .iter()
            .any(|c| c.id == "revoked" && c.revoked));
        drop(migrated);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn dedup_conflict_and_restart() {
        let p = std::env::temp_dir().join(format!("maxa-store-{}.sqlite", id()));
        let c = Invocation {
            request_id: id(),
            chat_id: "chat".into(),
            operation: "write".into(),
            arguments: json!({"content":"a"}),
        };
        {
            let s = Store::open(&p).unwrap();
            assert!(s.claim("c", &c).unwrap().is_none());
            assert_eq!(s.claim("c", &c).unwrap().unwrap()["state"], "running");
            let mut changed = c.clone();
            changed.arguments = json!({"content":"b"});
            assert!(s.claim("c", &changed).is_err());
        }
        {
            let restarted = Store::open(&p).unwrap();
            assert_eq!(
                restarted.claim("c", &c).unwrap().unwrap()["state"],
                "unknown"
            );
            assert!(restarted.request("c", &c.request_id, "other-chat").is_err());
        }
        let _ = std::fs::remove_file(p);
    }
}
