use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    // Release builds ask for UAC on double-click; debug builds stay
    // non-elevated so tests can run them (UC_NO_TUN).
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        embed_manifest(
            new_manifest("UberConnector").requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
