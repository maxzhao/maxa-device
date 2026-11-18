fn main() {
    #[cfg(all(windows, feature = "desktop"))]
    {
        use sha2::{Digest, Sha256};
        let version = std::env::var("CARGO_PKG_VERSION").expect("package version");
        let name = format!("maxa-device-companion-{version}.apk");
        let path = std::path::Path::new("../android/dist/companion").join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path)
            .expect("Build same-version companion APK before the Windows installer");
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let manifest = format!(r#"{{"file":"{name}","version":"{version}","sha256":"{sha256}"}}"#);
        std::fs::write(
            std::path::Path::new(&std::env::var("OUT_DIR").unwrap())
                .join("companion-manifest.json"),
            &manifest,
        )
        .expect("write build manifest");
        println!("cargo:rustc-env=MAXA_COMPANION_MANIFEST={manifest}");
        tauri_build::build();
    }
}
