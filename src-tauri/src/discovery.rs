use crate::model::*;
use mdns_sd::{ServiceDaemon, ServiceInfo};
pub struct Advertisement {
    daemon: ServiceDaemon,
    name: String,
}
impl Advertisement {
    pub fn publish(identity: &Identity, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        let instance = format!("maxa-{}", identity.host_id);
        let hostname = format!("{}.local.", instance);
        let properties = [
            ("host_id", identity.host_id.as_str()),
            ("display_name", identity.display_name.as_str()),
            ("protocol", "2"),
            ("system", "windows-x64"),
        ];
        let info = ServiceInfo::new(
            "_maxa-device._tcp.local.",
            &instance,
            &hostname,
            "",
            port,
            &properties[..],
        )
        .map_err(|e| e.to_string())?
        .enable_addr_auto();
        let name = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| e.to_string())?;
        Ok(Self { daemon, name })
    }
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.name);
        let _ = self.daemon.shutdown();
    }
}
