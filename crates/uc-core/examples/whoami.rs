//! Prints an identity's ids (diagnostic).
//! Usage: cargo run -p uc-core --example whoami -- <uc-data dir>

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).expect("dossier uc-data");
    let me = uc_core::identity::Identity::load_or_create(std::path::Path::new(&dir))?;
    println!("{} {} {}", me.id, me.public_key_hex(), me.x25519_public_hex());
    Ok(())
}
