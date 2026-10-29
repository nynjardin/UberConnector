//! Persistent local identity (ed25519 key pair), stored next to the executable.

use anyhow::Result;
use mainline::SigningKey;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Checks a signature made by `Identity::sign_hex`, and that `id` belongs to `pk`.
pub fn verify(id: &str, pk_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let (mut pk, mut sig) = ([0u8; 32], [0u8; 64]);
    if hex::decode_to_slice(pk_hex, &mut pk).is_err() || hex::decode_to_slice(sig_hex, &mut sig).is_err() {
        return false;
    }
    if hex::encode(&pk[..8]) != id {
        return false;
    }
    VerifyingKey::from_bytes(&pk).is_ok_and(|k| k.verify(msg, &Signature::from_bytes(&sig)).is_ok())
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Identity {
    /// First 8 bytes of the public key (hex): slot ownership, virtual IP
    /// derivation, deciding who dials whom.
    pub id: String,
    pub name: String,
    /// Ed25519 secret key (hex). Owning a salon = holding this key.
    #[serde(default)]
    secret: String,
}

impl Identity {
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("identity.json");
        let mut ident: Identity = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| Identity {
                id: String::new(),
                name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "joueur".into()),
                secret: String::new(),
            });
        // Prototype identities had no key: give them one.
        if ident.secret.len() != 64 {
            ident.secret = hex::encode(rand::random::<[u8; 32]>());
            ident.id = hex::encode(&ident.public_key()[..8]);
            ident.save(data_dir)?;
        }
        Ok(ident)
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::write(data_dir.join("identity.json"), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn signing_key(&self) -> SigningKey {
        let mut b = [0u8; 32];
        hex::decode_to_slice(&self.secret, &mut b).expect("identity secret");
        SigningKey::from_bytes(&b)
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key().verifying_key().to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key())
    }

    /// X25519 secret, derived from (not equal to) the signing key: used to
    /// receive a salon's new access key after a ban.
    fn x25519_secret(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::new().chain_update(b"uc-x25519/").chain_update(self.secret.as_bytes()).finalize().into()
    }

    pub fn x25519_public_hex(&self) -> String {
        hex::encode(curve25519_dalek::MontgomeryPoint::mul_base_clamped(self.x25519_secret()).to_bytes())
    }

    /// Symmetric key shared with the holder of `their_x25519` (hex).
    pub fn shared_key(&self, their_x25519: &str, context: &str) -> anyhow::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut pk = [0u8; 32];
        hex::decode_to_slice(their_x25519, &mut pk)?;
        let shared = curve25519_dalek::MontgomeryPoint(pk).mul_clamped(self.x25519_secret());
        Ok(Sha256::new()
            .chain_update(b"uc-shared/")
            .chain_update(context.as_bytes())
            .chain_update(shared.to_bytes())
            .finalize()
            .into())
    }

    /// Hex signature of `msg` with the identity key.
    pub fn sign_hex(&self, msg: &[u8]) -> String {
        use ed25519_dalek::Signer;
        hex::encode(self.signing_key().sign(msg).to_bytes())
    }

    /// EasyTier wants a UUID-shaped machine id.
    pub fn machine_uuid(&self) -> String {
        let h = format!("{:0<32}", self.id);
        format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    }
}
