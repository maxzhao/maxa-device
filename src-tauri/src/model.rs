use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    pub revoked: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub id: String,
    pub client_name: String,
    pub client_identity: String,
    pub verification_code: String,
    pub created_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleStatus {
    pub id: String,
    pub phase: String,
    pub version: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: String,
    pub request_id: String,
    pub client_id: String,
    pub chat_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub created_at: u64,
    pub expires_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneInfo {
    #[serde(default)]
    pub removed: bool,
    pub id: String,
    pub name: String,
    pub transports: Vec<String>,
    pub active_transport: Option<String>,
    pub state: String,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlInfo {
    pub id: String,
    pub module: String,
    pub resource: String,
    pub client_id: String,
    pub chat_id: String,
    pub operation: String,
    pub started_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub host_id: String,
    pub display_name: String,
    pub public_key_pin: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    pub state: String,
    pub address: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSnapshot {
    pub protocol_version: u32,
    pub identity: Identity,
    pub service: ServiceInfo,
    pub modules: Vec<ModuleStatus>,
    pub clients: Vec<ClientInfo>,
    pub pairing: Vec<PairRequest>,
    pub phones: Vec<PhoneInfo>,
    pub controls: Vec<ControlInfo>,
    pub jobs: Vec<JobInfo>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invocation {
    pub request_id: String,
    pub chat_id: String,
    pub operation: String,
    pub arguments: Value,
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}
pub type Result<T> = std::result::Result<T, String>;
