fn main() {
    // On macOS, embed Info.plist into __TEXT,__info_plist so that TCC
    // (Local Network privacy, etc.) treats this CLI binary as having
    // its own bundle metadata — required for grants to persist under
    // SIP-enabled systems. See Apple TN3179.
    #[cfg(target_os = "macos")]
    {
        use std::path::PathBuf;
        let version = env!("CARGO_PKG_VERSION");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>net.calii.fucina</string>
<key>CFBundleName</key><string>fucina</string>
<key>CFBundleExecutable</key><string>fucina</string>
<key>CFBundleVersion</key><string>{v}</string>
<key>CFBundleShortVersionString</key><string>{v}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
<key>NSLocalNetworkUsageDescription</key><string>Fucina connects to your Gitea server on the local network to poll for CI jobs.</string>
<key>NSBonjourServices</key><array><string>_http._tcp</string><string>_https._tcp</string></array>
</dict></plist>"#,
            v = version
        );
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("Info.plist");
        std::fs::write(&out, plist).unwrap();
        println!("cargo:rustc-link-arg=-sectcreate");
        println!("cargo:rustc-link-arg=__TEXT");
        println!("cargo:rustc-link-arg=__info_plist");
        println!("cargo:rustc-link-arg={}", out.display());
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=Cargo.toml");
    }
}
