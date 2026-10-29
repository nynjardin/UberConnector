//! JSON-lines protocol between the (non-elevated) app and the elevated
//! helper over a loopback TCP connection opened by the helper.

use crate::chat::ChatLine;
use crate::salons::Member;
use crate::session::{Profile, Status};
use serde::{Deserialize, Serialize};

/// App -> helper.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Sent first and on every change; applies live to all sessions.
    Profile(Profile),
    /// Connects to a salon (several can be connected at once).
    Join {
        salon_id: String,
        /// Hex secret access key of the salon.
        access_key: String,
        relays: Vec<String>,
        /// Virtual adapter (game mode); otherwise chat/presence only.
        game: bool,
        #[serde(default)]
        banned: Vec<String>,
        /// Open EasyTier's port on the router (NAT-PMP / UPnP).
        #[serde(default = "yes")]
        port_map: bool,
    },
    SetBanned { salon_id: String, banned: Vec<String> },
    /// Game mode: test the virtual LAN of this salon.
    GameTest { salon_id: String },
    Leave { salon_id: String },
    Say { salon_id: String, text: String },
    /// Ask the members of every connected salon for a missing image.
    WantImage { hash: String },
    /// Owner changed the salon: tell its members to refetch the manifest.
    NotifySalonChanged { salon_id: String },
    Quit,
}

fn yes() -> bool {
    true
}

/// Helper -> app.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First line: proves the helper was launched by this app.
    Hello { token: String },
    Status(Status),
    Chat { salon_id: String, line: ChatLine },
    /// An image is now in the local cache (`images`).
    ImageReady { hash: String },
    /// A member relayed the owner's "salon changed" hint.
    SalonChanged { salon_id: String },
    /// A member proved its identity in the chat (roster for key rotation).
    Member { salon_id: String, member: Member },
    Left { salon_id: String },
    Error { salon_id: Option<String>, message: String },
}
