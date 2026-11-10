use std::process::Command;

fn main() {
    // Embed the compiler version for `kumapulse_build_info`. Uses `rustc -vV`
    // so the exact toolchain that produced a binary is queryable from the
    // metrics endpoint, which matters when a rebuilt image behaves differently.
    let rustc = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map_or_else(
            || "unknown".to_string(),
            |text| text.trim().replace('"', ""),
        );
    println!("cargo:rustc-env=KUMAPULSE_RUSTC={rustc}");
    println!("cargo:rerun-if-changed=build.rs");
}
