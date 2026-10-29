//! Persistent salons, without any server.
//!
//! - The **manifest** (name, avatar, deleted flag, moderators, owner's ban
//!   list, key epoch) is a BEP44 mutable item under the owner's identity key:
//!   only the owner can write it.
//! - Each **moderator** publishes its own item (ban list, key epoch) under
//!   its identity key; it counts only while the owner lists that moderator.
//! - Manifest and moderator items are encrypted with the salon's *base key*
//!   (its first access key, never rotated): readable by anyone who was ever
//!   invited, which is fine for this metadata.
//! - The **access key** (EasyTier network + presence) is rotated on a ban or
//!   on demand: the new key is put in the DHT once per remaining member,
//!   encrypted for that member's X25519 key (from the roster, learnt over
//!   signed chat hellos). A banned member gets nothing.
//! - The **invite record** (under a key derived from the code) gives the
//!   current key to newcomers; rotating also replaces the code.
//! - Members republish the signed items as-is, so a salon survives while its
//!   owner is offline. BEP44 items expire after ~2 h without republishing.
//! - **Gated** salons (owner's choice): the invite record carries no access
//!   key. Newcomers knock in a waiting room (DHT slots under the base key);
//!   the owner or a moderator admits them by putting the current key,
//!   encrypted for them, under its own identity key.

use crate::identity::Identity;
use crate::room::{self, InviteKeys, Room};
use anyhow::{Context, Result, bail};
use futures::future::join_all;
use mainline::async_dht::AsyncDht;
use mainline::{Dht, MutableItem};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Keeps the manifest under the ~1000 byte BEP44 limit.
pub const MAX_MODS: usize = 4;
pub const MAX_OWNER_BANS: usize = 20;
pub const MAX_MOD_BANS: usize = 40;

/// A member as seen in signed chat hellos.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Member {
    pub id: String,
    /// Ed25519 identity key (hex).
    pub pk: String,
    /// X25519 key (hex) to receive rotated access keys.
    pub xpk: String,
    pub name: String,
}

/// A salon as stored locally (`uc-data/salons.json`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Salon {
    pub id: String,
    pub name: String,
    /// Owner public key (hex).
    pub owner: String,
    /// Current secret access key (hex).
    pub access_key: String,
    pub invite: String,
    #[serde(default)]
    pub deleted: bool,
    /// "emoji|#rrggbb", "img:<sha256>" or "" (initials).
    #[serde(default)]
    pub avatar: String,
    /// First access key (hex), encrypts manifest and moderator items.
    /// Empty for salons created before key rotation existed: = access_key.
    #[serde(default)]
    pub base_key: String,
    /// Access key generation.
    #[serde(default)]
    pub epoch: u64,
    /// Last epoch *we* announced (owner or moderator); 0 if none.
    #[serde(default)]
    pub my_epoch: u64,
    /// Moderators' public keys (from the manifest).
    #[serde(default)]
    pub mods: Vec<String>,
    /// Owner's own ban list (from the manifest).
    #[serde(default)]
    pub owner_banned: Vec<String>,
    /// Our own ban list, when we are a moderator.
    #[serde(default)]
    pub my_banned: Vec<String>,
    /// Effective ban list (owner + valid moderators), member ids.
    #[serde(default)]
    pub banned: Vec<String>,
    /// Members met in the chat (to hand them rotated keys).
    #[serde(default)]
    pub roster: Vec<Member>,
    /// A newer key exists but none was left for us: a new invite is needed.
    #[serde(default)]
    pub key_lost: bool,
    /// Newcomers must be admitted by the owner or a moderator (manifest).
    #[serde(default)]
    pub gated: bool,
    /// We wait for admission: `access_key` is not usable yet.
    #[serde(default)]
    pub pending: bool,
}

impl Salon {
    fn key_bytes(hex_key: &str) -> [u8; 32] {
        let mut b = [0u8; 32];
        hex::decode_to_slice(hex_key, &mut b).expect("salon key");
        b
    }

    pub fn access_key_bytes(&self) -> [u8; 32] {
        Self::key_bytes(&self.access_key)
    }

    /// Network credentials for the current key.
    pub fn room(&self) -> Room {
        Room::new(&self.id, &self.access_key_bytes())
    }

    /// Never rotates: reads manifest and moderator items.
    fn base_room(&self) -> Room {
        let base = if self.base_key.is_empty() { &self.access_key } else { &self.base_key };
        Room::new(&self.id, &Self::key_bytes(base))
    }

    pub fn subnet(&self) -> u8 {
        subnet_for(&self.id)
    }

    pub fn is_owned_by(&self, ident: &Identity) -> bool {
        self.owner == ident.public_key_hex()
    }

    pub fn is_mod(&self, ident: &Identity) -> bool {
        self.mods.contains(&ident.public_key_hex())
    }

    pub fn can_moderate(&self, ident: &Identity) -> bool {
        self.is_owned_by(ident) || self.is_mod(ident)
    }

    /// Records a member met in the chat (signature already checked).
    pub fn remember(&mut self, m: Member) -> bool {
        match self.roster.iter_mut().find(|x| x.id == m.id) {
            Some(x) if *x == m => false,
            Some(x) => {
                *x = m;
                true
            }
            None => {
                self.roster.push(m);
                true
            }
        }
    }

    fn salt(&self) -> Vec<u8> {
        format!("uc-salon/{}", self.id).into_bytes()
    }

    fn mod_salt(&self) -> Vec<u8> {
        format!("uc-salon/{}/mod", self.id).into_bytes()
    }

    fn rekey_salt(&self, epoch: u64, member: &str) -> Vec<u8> {
        format!("uc-rekey/{}/{epoch}/{member}", self.id).into_bytes()
    }

    fn rekey_context(&self, epoch: u64) -> String {
        format!("{}/{epoch}", self.id)
    }

    fn admit_salt(&self, member: &str) -> Vec<u8> {
        format!("uc-admit/{}/{member}", self.id).into_bytes()
    }

    fn admit_context(&self) -> String {
        format!("admit/{}", self.id)
    }

    /// Waiting room: readable by anyone who was ever invited.
    fn lobby_room(&self) -> Room {
        let base = if self.base_key.is_empty() { &self.access_key } else { &self.base_key };
        Room::new(&format!("{}/lobby", self.id), &Self::key_bytes(base))
    }
}

/// A newcomer waiting for admission to a gated salon (signed by its identity).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Knock {
    pub id: String,
    pub pk: String,
    pub xpk: String,
    pub name: String,
    pub avatar: String,
    pub ts: u64,
    pub sig: String,
}

impl Knock {
    fn msg(salon_id: &str, id: &str, xpk: &str, ts: u64) -> Vec<u8> {
        format!("uc-knock/{salon_id}/{id}/{xpk}/{ts}").into_bytes()
    }

    pub fn member(&self) -> Member {
        Member { id: self.id.clone(), pk: self.pk.clone(), xpk: self.xpk.clone(), name: self.name.clone() }
    }
}

const LOBBY_SLOTS: usize = 8;
/// A knock not refreshed for this long is gone (newcomers refresh every minute).
pub const KNOCK_STALE_SECS: u64 = 180;

fn lobby_salt(i: usize) -> Vec<u8> {
    format!("uc-knock-{i}").into_bytes()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    id: String,
    name: String,
    deleted: bool,
    #[serde(default)]
    avatar: String,
    #[serde(default)]
    mods: Vec<String>,
    #[serde(default)]
    banned: Vec<String>,
    /// Epoch announced by the owner, and the owner's X25519 key to decrypt it.
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    key_by_x: String,
    #[serde(default)]
    gated: bool,
}

#[derive(Serialize, Deserialize)]
struct ModItem {
    banned: Vec<String>,
    epoch: u64,
    key_by_x: String,
}

#[derive(Serialize, Deserialize)]
struct Rekey {
    epoch: u64,
    key: String,
    invite: String,
}

#[derive(Serialize, Deserialize)]
struct InviteRecord {
    id: String,
    owner: String,
    access_key: String,
    name: String,
    #[serde(default)]
    avatar: String,
    #[serde(default)]
    base_key: String,
    #[serde(default)]
    epoch: u64,
}

/// Second octet of the salon's 10.X.0.0/16 network. Derived from the id so
/// every member agrees and two salons rarely share it (range 30..=229).
pub fn subnet_for(salon_id: &str) -> u8 {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("uc-subnet/{salon_id}").as_bytes());
    30 + h[0] % 200
}

pub fn load(data_dir: &Path) -> Vec<Salon> {
    std::fs::read_to_string(data_dir.join("salons.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, salons: &[Salon]) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_dir.join("salons.json"), serde_json::to_string_pretty(salons)?)?;
    Ok(())
}

pub fn create(owner: &Identity, name: &str) -> Salon {
    let key = hex::encode(rand::random::<[u8; 32]>());
    Salon {
        id: hex::encode(rand::random::<[u8; 8]>()),
        name: name.trim().to_string(),
        owner: owner.public_key_hex(),
        access_key: key.clone(),
        invite: room::generate_code(),
        deleted: false,
        avatar: String::new(),
        base_key: key,
        epoch: 0,
        my_epoch: 0,
        mods: vec![],
        owner_banned: vec![],
        my_banned: vec![],
        banned: vec![],
        roster: vec![],
        key_lost: false,
        gated: false,
        pending: false,
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let mut b = [0u8; 32];
    hex::decode_to_slice(s, &mut b).context("clé invalide")?;
    Ok(b)
}

pub enum Refresh {
    Unchanged,
    /// Something changed (name, avatar, roles, bans, access key...).
    Updated,
}

pub struct Directory {
    dht: AsyncDht,
}

impl Directory {
    pub async fn start() -> Result<Self> {
        let dht = Dht::client()?.as_async();
        if !dht.bootstrapped().await {
            bail!("impossible de rejoindre la DHT (pas d'accès Internet ?)");
        }
        Ok(Self { dht })
    }

    /// Like `start`, from (and remembering) the DHT nodes known in `data_dir`.
    pub async fn start_in(data_dir: &Path) -> Result<Self> {
        let dht = crate::cache::dht(Some(data_dir))?;
        if !dht.bootstrapped().await {
            bail!("impossible de rejoindre la DHT (pas d'accès Internet ?)");
        }
        crate::cache::save_dht_nodes(&dht, data_dir).await;
        Ok(Self { dht })
    }

    async fn put(&self, signer: mainline::SigningKey, salt: Vec<u8>, value: Vec<u8>) -> Result<()> {
        let item = MutableItem::new(signer, &value, now_millis(), Some(&salt));
        self.dht.put_mutable(item, None).await?;
        Ok(())
    }

    /// Owner only: (re)publishes the manifest and, unless deleted, the invite.
    pub async fn publish(&self, owner: &Identity, salon: &Salon) -> Result<()> {
        if !salon.is_owned_by(owner) {
            bail!("seul le propriétaire peut modifier ce salon");
        }
        let manifest = Manifest {
            id: salon.id.clone(),
            name: salon.name.clone(),
            deleted: salon.deleted,
            avatar: salon.avatar.clone(),
            mods: salon.mods.clone(),
            banned: salon.owner_banned.clone(),
            epoch: salon.my_epoch,
            key_by_x: owner.x25519_public_hex(),
            gated: salon.gated,
        };
        let value = salon.base_room().encrypt(&serde_json::to_vec(&manifest)?);
        if value.len() > 990 {
            bail!("trop de modérateurs ou de bannis pour la fiche du salon");
        }
        self.put(owner.signing_key(), salon.salt(), value).await?;
        self.publish_invite(salon, &salon.invite, salon.deleted).await
    }

    /// Moderator only: (re)publishes our ban list and announced epoch.
    pub async fn publish_mod(&self, me: &Identity, salon: &Salon) -> Result<()> {
        if !salon.is_mod(me) {
            bail!("vous n'êtes pas modérateur de ce salon");
        }
        let item = ModItem { banned: salon.my_banned.clone(), epoch: salon.my_epoch, key_by_x: me.x25519_public_hex() };
        let value = salon.base_room().encrypt(&serde_json::to_vec(&item)?);
        self.put(me.signing_key(), salon.mod_salt(), value).await
    }

    async fn publish_invite(&self, salon: &Salon, code: &str, revoked: bool) -> Result<()> {
        let keys = InviteKeys::from_code(code)?;
        // A revoked invite is overwritten with an empty record; a gated one
        // has no access key (newcomers must be admitted).
        let plain = if revoked {
            b"{}".to_vec()
        } else {
            serde_json::to_vec(&InviteRecord {
                id: salon.id.clone(),
                owner: salon.owner.clone(),
                access_key: if salon.gated { String::new() } else { salon.access_key.clone() },
                name: salon.name.clone(),
                avatar: salon.avatar.clone(),
                base_key: if salon.base_key.is_empty() { salon.access_key.clone() } else { salon.base_key.clone() },
                epoch: salon.epoch,
            })?
        };
        let value = room::encrypt(&keys.enc_key, &plain);
        let item = MutableItem::new(keys.signing_key, &value, now_millis(), None);
        self.dht.put_mutable(item, None).await?;
        Ok(())
    }

    /// Owner or moderator: new access key + new invite code, handed to every
    /// roster member except the banned ones. Updates `salon` in place.
    pub async fn rotate(&self, me: &Identity, salon: &mut Salon) -> Result<usize> {
        if !salon.can_moderate(me) {
            bail!("seuls le propriétaire et les modérateurs peuvent faire ça");
        }
        let old_invite = salon.invite.clone();
        salon.access_key = hex::encode(rand::random::<[u8; 32]>());
        salon.invite = room::generate_code();
        salon.epoch += 1;
        salon.my_epoch = salon.epoch;
        salon.key_lost = false;
        let sent = self.publish_rekeys(me, salon).await;
        self.publish_invite(salon, &old_invite, true).await?;
        if salon.is_owned_by(me) {
            self.publish(me, salon).await?;
        } else {
            self.publish_invite(salon, &salon.invite, false).await?;
            self.publish_mod(me, salon).await?;
        }
        Ok(sent)
    }

    /// Puts the current key for each roster member (we must be the announcer).
    async fn publish_rekeys(&self, me: &Identity, salon: &Salon) -> usize {
        let payload = Rekey { epoch: salon.epoch, key: salon.access_key.clone(), invite: salon.invite.clone() };
        let plain = serde_json::to_vec(&payload).unwrap();
        let ctx = salon.rekey_context(salon.epoch);
        let puts = salon
            .roster
            .iter()
            .filter(|m| m.id != me.id && !salon.banned.contains(&m.id))
            .filter_map(|m| {
                let key = me.shared_key(&m.xpk, &ctx).ok()?;
                let salt = salon.rekey_salt(salon.epoch, &m.id);
                Some(self.put(me.signing_key(), salt, room::encrypt(&key, &plain)))
            });
        join_all(puts).await.into_iter().filter(|r| r.is_ok()).count()
    }

    async fn fetch(&self, pk_hex: &str, salt: &[u8]) -> Result<Option<MutableItem>> {
        Ok(self.dht.get_mutable_most_recent(&hex32(pk_hex)?, Some(salt)).await)
    }

    /// Looks up an invite code, then reads the salon's current state.
    pub async fn resolve(&self, me: &Identity, code: &str) -> Result<Salon> {
        let keys = InviteKeys::from_code(code)?;
        let pk = keys.signing_key.verifying_key().to_bytes();
        let item = self
            .dht
            .get_mutable_most_recent(&pk, None)
            .await
            .context("code d'invitation introuvable (salon inexistant, ou aucun membre en ligne depuis plus de 2 h)")?;
        let plain = room::decrypt(&keys.enc_key, item.value()).context("code d'invitation invalide")?;
        let rec: InviteRecord = serde_json::from_slice(&plain).context("cette invitation a été révoquée")?;
        let gated = rec.access_key.is_empty();
        let base_key = if rec.base_key.is_empty() { rec.access_key.clone() } else { rec.base_key };
        hex32(&base_key)?;
        let mut salon = Salon {
            id: rec.id,
            name: rec.name,
            owner: rec.owner,
            // Placeholder while pending (a valid key, never used to connect).
            access_key: if gated { base_key.clone() } else { rec.access_key },
            invite: keys.code,
            deleted: false,
            avatar: rec.avatar,
            base_key,
            epoch: rec.epoch,
            my_epoch: 0,
            mods: vec![],
            owner_banned: vec![],
            my_banned: vec![],
            banned: vec![],
            roster: vec![],
            key_lost: false,
            gated,
            pending: gated,
        };
        self.refresh(me, &mut salon).await?;
        if salon.deleted {
            bail!("ce salon a été supprimé par son propriétaire");
        }
        if salon.banned.contains(&me.id) {
            bail!("vous êtes banni de ce salon");
        }
        Ok(salon)
    }

    /// Keeps a salon alive in the DHT, applies the owner's and moderators'
    /// latest changes and picks up a rotated access key.
    pub async fn refresh(&self, me: &Identity, salon: &mut Salon) -> Result<Refresh> {
        let before = serde_json::to_string(&*salon)?;
        let base = salon.base_room();

        // Owner's manifest.
        if let Some(item) = self.fetch(&salon.owner, &salon.salt()).await? {
            let m: Manifest = serde_json::from_slice(&base.decrypt(item.value())?)?;
            if m.id != salon.id {
                bail!("fiche de salon incohérente");
            }
            if !salon.is_owned_by(me) {
                let _ = self.dht.put_mutable(item, None).await;
            }
            salon.name = m.name;
            salon.deleted = m.deleted;
            salon.avatar = m.avatar;
            salon.mods = m.mods;
            salon.owner_banned = m.banned;
            salon.gated = m.gated;
            let mut best = (m.epoch, salon.owner.clone(), m.key_by_x);

            // Moderators' items (only those the owner lists).
            let mut banned = salon.owner_banned.clone();
            for pk in salon.mods.clone() {
                let Ok(Some(item)) = self.fetch(&pk, &salon.mod_salt()).await else { continue };
                let Ok(plain) = base.decrypt(item.value()) else { continue };
                let Ok(mi) = serde_json::from_slice::<ModItem>(&plain) else { continue };
                if pk != me.public_key_hex() {
                    let _ = self.dht.put_mutable(item, None).await;
                }
                banned.extend(mi.banned);
                if mi.epoch > best.0 {
                    best = (mi.epoch, pk, mi.key_by_x);
                }
            }
            banned.sort();
            banned.dedup();
            salon.banned = banned;
            if !salon.is_mod(me) {
                salon.my_banned.clear();
            }

            // A newer access key was announced: fetch the copy left for us.
            let (epoch, by, by_x) = best;
            if epoch > salon.epoch && by != me.public_key_hex() && !salon.pending {
                match self.fetch(&by, &salon.rekey_salt(epoch, &me.id)).await? {
                    Some(item) => {
                        let key = me.shared_key(&by_x, &salon.rekey_context(epoch))?;
                        let r: Rekey = serde_json::from_slice(&room::decrypt(&key, item.value())?)?;
                        hex32(&r.key)?;
                        salon.access_key = r.key;
                        salon.invite = r.invite;
                        salon.epoch = r.epoch;
                        salon.key_lost = false;
                    }
                    None => {
                        salon.key_lost = true;
                        // Gated: only an admission at this epoch or later helps.
                        if salon.gated {
                            salon.epoch = epoch;
                        }
                    }
                }
            }
        }

        // Our own duties: owner/moderator items, keys we handed out, invite.
        if salon.is_owned_by(me) {
            self.publish(me, salon).await?;
        } else if salon.is_mod(me) {
            self.publish_mod(me, salon).await?;
        }
        if salon.my_epoch > 0 && salon.my_epoch == salon.epoch {
            self.publish_rekeys(me, salon).await;
        }
        // Lost the key of a gated salon: back to the waiting room.
        if salon.key_lost && salon.gated {
            salon.key_lost = false;
            salon.pending = true;
        }
        if !salon.deleted && !salon.key_lost && !salon.pending && !salon.is_owned_by(me) {
            let _ = self.publish_invite(salon, &salon.invite, false).await;
        }

        let changed = serde_json::to_string(&*salon)? != before;
        Ok(if changed { Refresh::Updated } else { Refresh::Unchanged })
    }

    async fn read_lobby(&self, salon: &Salon) -> Vec<Option<Knock>> {
        let lobby = salon.lobby_room();
        let key = lobby.dht_public_key();
        let futs = (0..LOBBY_SLOTS).map(|i| {
            let s = lobby_salt(i);
            async move { self.dht.get_mutable_most_recent(&key, Some(&s)).await }
        });
        join_all(futs)
            .await
            .into_iter()
            .map(|item| {
                let k: Knock = serde_json::from_slice(&lobby.decrypt(item?.value()).ok()?).ok()?;
                let ok = crate::identity::verify(&k.id, &k.pk, &Knock::msg(&salon.id, &k.id, &k.xpk, k.ts), &k.sig);
                ok.then_some(k)
            })
            .collect()
    }

    /// Newcomer: (re)writes our knock in the waiting room.
    pub async fn knock(&self, me: &Identity, salon: &Salon, name: &str, avatar: &str) -> Result<()> {
        let slots = self.read_lobby(salon).await;
        let now = now_secs();
        let slot = slots.iter().position(|k| k.as_ref().is_some_and(|k| k.id == me.id)).unwrap_or_else(|| {
            let free: Vec<usize> = (0..LOBBY_SLOTS)
                .filter(|&i| slots[i].as_ref().is_none_or(|k| now.saturating_sub(k.ts) > KNOCK_STALE_SECS))
                .collect();
            if free.is_empty() { rand::random_range(0..LOBBY_SLOTS) } else { free[rand::random_range(0..free.len())] }
        });
        let xpk = me.x25519_public_hex();
        let knock = Knock {
            sig: me.sign_hex(&Knock::msg(&salon.id, &me.id, &xpk, now)),
            id: me.id.clone(),
            pk: me.public_key_hex(),
            xpk,
            name: name.chars().take(32).collect(),
            avatar: avatar.chars().take(72).collect(),
            ts: now,
        };
        let lobby = salon.lobby_room();
        let value = lobby.encrypt(&serde_json::to_vec(&knock)?);
        let item = MutableItem::new(lobby.signing_key, &value, now_millis(), Some(&lobby_salt(slot)));
        self.dht.put_mutable(item, None).await?;
        Ok(())
    }

    /// Owner or moderator: newcomers currently waiting (banned ones left out).
    pub async fn knocks(&self, salon: &Salon) -> Vec<Knock> {
        let now = now_secs();
        let mut out: Vec<Knock> = self
            .read_lobby(salon)
            .await
            .into_iter()
            .flatten()
            .filter(|k| now.saturating_sub(k.ts) <= KNOCK_STALE_SECS && k.ts <= now + 60)
            .filter(|k| !salon.banned.contains(&k.id))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }

    /// Owner or moderator: hands the current key to a newcomer and adds it to
    /// our roster (so it also gets the next rotated key from us).
    pub async fn admit(&self, me: &Identity, salon: &mut Salon, knock: &Knock) -> Result<()> {
        if !salon.can_moderate(me) {
            bail!("seuls le propriétaire et les modérateurs peuvent faire ça");
        }
        if salon.banned.contains(&knock.id) {
            bail!("ce membre est banni du salon");
        }
        let payload = Rekey { epoch: salon.epoch, key: salon.access_key.clone(), invite: salon.invite.clone() };
        let key = me.shared_key(&knock.xpk, &salon.admit_context())?;
        // Our X25519 key in clear (authenticated by the BEP44 signature).
        let mut value = hex::decode(me.x25519_public_hex())?;
        value.extend(room::encrypt(&key, &serde_json::to_vec(&payload)?));
        self.put(me.signing_key(), salon.admit_salt(&knock.id), value).await?;
        salon.remember(knock.member());
        Ok(())
    }

    /// Newcomer: looks for an admission from the owner or a moderator, or
    /// for the key in the invite if the owner opened the salon meanwhile.
    /// Returns true once `salon` holds a usable key.
    pub async fn check_admission(&self, me: &Identity, salon: &mut Salon) -> Result<bool> {
        if !salon.gated {
            let keys = InviteKeys::from_code(&salon.invite)?;
            let pk = keys.signing_key.verifying_key().to_bytes();
            if let Some(item) = self.dht.get_mutable_most_recent(&pk, None).await {
                let plain = room::decrypt(&keys.enc_key, item.value())?;
                if let Ok(rec) = serde_json::from_slice::<InviteRecord>(&plain) {
                    if rec.id == salon.id && hex32(&rec.access_key).is_ok() {
                        salon.access_key = rec.access_key;
                        salon.epoch = rec.epoch;
                        salon.pending = false;
                        return Ok(true);
                    }
                }
            }
        }
        let by: Vec<String> = std::iter::once(salon.owner.clone()).chain(salon.mods.iter().cloned()).collect();
        let salt = salon.admit_salt(&me.id);
        let items = join_all(by.iter().map(|pk| self.fetch(pk, &salt))).await;
        let best = items
            .into_iter()
            .filter_map(|r| r.ok().flatten())
            .filter_map(|item| {
                let v = item.value();
                let by_x = hex::encode(v.get(..32)?);
                let key = me.shared_key(&by_x, &salon.admit_context()).ok()?;
                let r: Rekey = serde_json::from_slice(&room::decrypt(&key, &v[32..]).ok()?).ok()?;
                hex32(&r.key).ok().map(|_| r)
            })
            // An older admission's key was rotated since.
            .filter(|r| r.epoch >= salon.epoch)
            .max_by_key(|r| r.epoch);
        let Some(r) = best else { return Ok(false) };
        salon.access_key = r.key;
        salon.invite = r.invite;
        salon.epoch = r.epoch;
        salon.pending = false;
        salon.key_lost = false;
        Ok(true)
    }
}
