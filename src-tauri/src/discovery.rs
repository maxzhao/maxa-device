use crate::model::*;
use mdns_sd::{ServiceDaemon, ServiceInfo};
pub struct Advertisement {
    daemon: ServiceDaemon,
    name: String,
    port: u16,
    display_name: String,
}
impl Advertisement {
    pub fn publish(identity: &Identity, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
        let info = service_info(identity, port)?;
        let name = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| e.to_string())?;
        Ok(Self {
            daemon,
            name,
            port,
            display_name: identity.display_name.clone(),
        })
    }
    pub fn update_identity(&mut self, identity: &Identity) -> Result<()> {
        if self.display_name != identity.display_name {
            // Registering the same fullname on this daemon updates and re-announces its TXT.
            self.daemon
                .register(service_info(identity, self.port)?)
                .map_err(|e| e.to_string())?;
            self.display_name = identity.display_name.clone();
        }
        Ok(())
    }
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.name);
        let _ = self.daemon.shutdown();
    }
}

pub fn build_system() -> &'static str {
    if cfg!(all(target_os = "android", target_arch = "aarch64")) {
        "android-arm64"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        std::env::consts::OS
    }
}

fn service_info(identity: &Identity, port: u16) -> Result<ServiceInfo> {
    let instance = format!("maxa-{}", identity.host_id);
    let hostname = format!("{}.local.", instance);
    let properties = [
        ("host_id", identity.host_id.as_str()),
        ("display_name", identity.display_name.as_str()),
        ("protocol", "2"),
        ("system", build_system()),
    ];
    ServiceInfo::new(
        "_maxa-device._tcp.local.",
        &instance,
        &hostname,
        "",
        port,
        &properties[..],
    )
    .map(ServiceInfo::enable_addr_auto)
    .map_err(|e| e.to_string())
}

#[cfg(test)]
#[test]
fn renamed_mdns_txt_preserves_host_identity() {
    let mut identity = Identity {
        host_id: id(),
        display_name: "Before".into(),
        public_key_pin: "test pin".into(),
    };
    let before = service_info(&identity, 19427).unwrap();
    identity.display_name = "我的平板".into();
    let after = service_info(&identity, 19427).unwrap();
    assert_eq!(after.get_fullname(), before.get_fullname());
    assert_eq!(
        after.get_property_val_str("host_id"),
        Some(identity.host_id.as_str())
    );
    assert_eq!(after.get_property_val_str("display_name"), Some("我的平板"));
    assert_eq!(after.get_property_val_str("system"), Some(build_system()));
}
