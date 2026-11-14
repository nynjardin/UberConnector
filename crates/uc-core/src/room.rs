//! Key derivation. A salon's network is derived from its id + secret access
//! key; an invite code only derives where (and how) to fetch those.

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use mainline::SigningKey;
use sha2::{Digest, Sha256};

/// No look-alikes: I/L/O and the digits 0 1 2 5 6 8 are excluded.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ3479";
const CODE_LEN: usize = 12;

fn derive(tag: &str, input: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"uberconnector/v1/");
    h.update(tag.as_bytes());
    h.update([0u8]);
    h.update(input);
    h.finalize().into()
}

pub fn encrypt(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let nonce_bytes: [u8; 12] = rand::random();
    let mut out = nonce_bytes.to_vec();
    out.extend(cipher.encrypt(&Nonce::from(nonce_bytes), plain).expect("chacha20 encrypt"));
    out
}

pub fn decrypt(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 12 {
        bail!("enregistrement trop court");
    }
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let nonce: [u8; 12] = data[..12].try_into()?;
    cipher.decrypt(&Nonce::from(nonce), &data[12..]).ok().context("déchiffrement impossible")
}

/// URL scheme registered by the app (per user) to open invite links.
pub const LINK_SCHEME: &str = "uberconnector";

/// Clickable form of an invite code: `uberconnector://join/K7QM-ZXRA-PQ4H`.
pub fn invite_link(code: &str) -> String {
    format!("{LINK_SCHEME}://join/{code}")
}

/// The code inside an invite link (browsers may add a trailing `/` or
/// percent-escapes), or inside a web invite page link (`https://…/#CODE`);
/// any other text is returned as is.
fn strip_link(text: &str) -> &str {
    let t = text.trim();
    if t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("http")) {
        return t.split_once('#').map_or("", |(_, code)| code);
    }
    // `get`: a byte index may fall inside a multi-byte character.
    let is_link = t.get(..LINK_SCHEME.len()).is_some_and(|p| p.eq_ignore_ascii_case(LINK_SCHEME))
        && t[LINK_SCHEME.len()..].starts_with(':');
    if !is_link {
        return t;
    }
    let path = t.split(['?', '#']).next().unwrap_or("");
    path.rsplit(['/', ':']).find(|s| !s.is_empty()).unwrap_or("")
}

fn normalize(code: &str) -> String {
    let code = strip_link(code);
    // Percent-escapes (`%20`, `%2D`…) only ever stand for separators.
    let mut plain = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(i) = rest.find('%') {
        plain.push_str(&rest[..i]);
        let esc = rest[i + 1..].get(..2).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
        rest = &rest[i + 1 + esc.map_or(0, |h| h.len())..];
    }
    plain.push_str(rest);
    plain
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            // A code read aloud or retyped may swap these.
            '5' => 'S',
            '2' => 'Z',
            '8' => 'B',
            '6' => 'G',
            c => c,
        })
        .collect()
}

fn format_code(raw: &str) -> String {
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

/// Random invite code such as `K7QM-ZXRA-PQ4H` (~57 bits of entropy).
pub fn generate_code() -> String {
    let raw: String = (0..CODE_LEN)
        .map(|_| CODE_ALPHABET[rand::random_range(0..CODE_ALPHABET.len())] as char)
        .collect();
    format_code(&raw)
}

/// Network credentials of a salon (what members actually connect with).
pub struct Room {
    /// Salon id (hex).
    pub id: String,
    pub network_name: String,
    pub network_secret: String,
    /// Key under which members publish their presence slots in the DHT.
    pub signing_key: SigningKey,
    enc_key: [u8; 32],
}

impl Room {
    pub fn new(salon_id: &str, access_key: &[u8; 32]) -> Self {
        let mut seed = salon_id.as_bytes().to_vec();
        seed.extend(access_key);
        Self {
            id: salon_id.to_string(),
            network_name: format!("uc-{}", &hex::encode(derive("name", &seed))[..20]),
            network_secret: hex::encode(derive("secret", &seed)),
            signing_key: SigningKey::from_bytes(&derive("dht-key", &seed)),
            enc_key: derive("enc", &seed),
        }
    }

    /// Authenticates chat (and later voice) connections between members.
    pub fn chat_key(&self) -> [u8; 32] {
        derive("chat", &self.enc_key)
    }

    /// Tag of the salon's presence messages on Nostr relays: reveals nothing
    /// and changes with the access key (a banned member no longer hears it).
    pub fn nostr_topic(&self) -> String {
        hex::encode(&derive("nostr-topic", &self.enc_key)[..16])
    }

    /// Swarm announced on the UDP trackers (see `tracker`): only members can
    /// compute it, and it says nothing about the salon.
    pub fn tracker_hash(&self) -> [u8; 20] {
        let d = derive("tracker-hash", &self.enc_key);
        let mut h = [0u8; 20];
        h.copy_from_slice(&d[..20]);
        h
    }

    pub fn dht_public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        encrypt(&self.enc_key, plain)
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        decrypt(&self.enc_key, data)
    }
}

/// Where an invite record lives in the DHT, and how it is encrypted.
pub struct InviteKeys {
    pub code: String,
    pub signing_key: SigningKey,
    pub enc_key: [u8; 32],
}

impl InviteKeys {
    pub fn from_code(code: &str) -> Result<Self> {
        let norm = normalize(code);
        if norm.len() < 8 {
            bail!("code d'invitation trop court (8 caractères minimum)");
        }
        Ok(Self {
            code: format_code(&norm),
            signing_key: SigningKey::from_bytes(&derive("invite-dht", norm.as_bytes())),
            enc_key: derive("invite-enc", norm.as_bytes()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_codes_and_crypto() {
        let code = generate_code();
        let a = InviteKeys::from_code(&code).unwrap();
        let b = InviteKeys::from_code(&code.to_lowercase().replace('-', " ")).unwrap();
        assert_eq!(a.code, b.code);
        assert_eq!(a.signing_key.to_bytes(), b.signing_key.to_bytes());
        let enc = encrypt(&a.enc_key, b"hello");
        assert_eq!(decrypt(&b.enc_key, &enc).unwrap(), b"hello");
        let other = InviteKeys::from_code("ZZZZ-ZZZZ-ZZZZ").unwrap();
        assert!(decrypt(&other.enc_key, &enc).is_err());
        let s = InviteKeys::from_code("FJNC-C92S-M9NJ").unwrap();
        let five = InviteKeys::from_code("fjnc c9z5 m9nj").unwrap();
        assert_eq!(s.code, five.code);
    }

    #[test]
    fn invite_links() {
        let code = "K7QM-ZXRA-PQ4H";
        for text in [
            invite_link(code),
            format!("{}/", invite_link(code)),
            "  UberConnector://join/k7qm%20zxra%2Dpq4h/?x=1 ".into(),
            "uberconnector:K7QMZXRAPQ4H".into(),
            "https://exemple.github.io/uc/#k7qm-zxra-pq4h".into(),
        ] {
            assert_eq!(InviteKeys::from_code(&text).unwrap().code, code, "{text}");
        }
        assert!(InviteKeys::from_code("uberconnector://join/").is_err());
        assert!(InviteKeys::from_code("https://exemple.github.io/uc/").is_err());
        // Byte 13 inside a multi-byte character: no panic.
        assert!(InviteKeys::from_code("aaaaaaaaaaaaé bbbb").is_ok());
        assert!(InviteKeys::from_code("hé").is_err());
    }

    #[test]
    fn room_depends_on_key() {
        let a = Room::new("abcd", &[1; 32]);
        let b = Room::new("abcd", &[2; 32]);
        assert_ne!(a.network_secret, b.network_secret);
        assert_ne!(a.dht_public_key(), b.dht_public_key());
    }
}
