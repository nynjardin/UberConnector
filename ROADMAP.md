# Roadmap and ideas

Nothing here is promised. It is the list of improvements considered so far,
with the reasoning behind each, so that they can be picked up later.

## Next, by order of interest

1. **No UAC prompt at each activation.** Windows asks for permission every
   time the app is switched on, because the helper needs administrator
   rights (virtual adapter, firewall, raw sockets). A scheduled task created
   once, with the user's consent, could start the helper elevated without a
   prompt afterwards. It is the most visible daily friction.
   Caution: the task would start the helper elevated without asking, so
   whatever can replace `uc-helper.exe` in the app folder (writable by the
   user, the app being portable) would get administrator rights silently.
   The helper should be copied to a protected folder (`Program Files`)
   when the task is created, which complicates updates a little.
2. **Signed updates.** The updater checks the SHA-256 digest GitHub
   publishes, so it trusts whatever the GitHub account serves. Signing the
   zip with a key kept off GitHub, and checking it in the app, would
   protect users if the account were compromised.

## Done from this list

- **Nostr fast path for salons** (after 0.2.0): admissions, knocks and
  salon changes (rename, bans, new key) are also pushed through Nostr as
  the same signed items the DHT holds (`uc_core::push`). Receivers check
  the signature and keep the most recent copy, so relays can only drop or
  replay items. Measured on the real network: an admission reaches the
  newcomer in 0.3 s (was 20 to 60 s), a rename in 0.3 s plus about 4 s of
  DHT reading. Only deliberate writes are pushed, never the 20-minute
  keepalives (two moderators would echo each other).
- **Network change during a session** (after 0.2.0): the session checks
  the LAN address every 5 s and the public one by STUN every 2 minutes;
  a change seen twice in a row rebuilds the session by itself (about 2 s,
  then members find each other the usual way).
- **Release build tuning** (after 0.2.0): thin LTO, one codegen unit,
  symbols stripped.
- **Fewer `easytier-cli` calls** (after 0.2.0): every second during the
  first minute of a session, then every 2 seconds.
- **Network secret off the command line** (after 0.2.0): easytier-core
  gets it through its environment (`ET_NETWORK_SECRET`), which other
  programs on the PC cannot read, unlike its command line.
- **Bridge on loopback** (after 0.2.0): the local socket between EasyTier
  and the hole-punching path listens on 127.0.0.1, so other machines on the
  LAN can no longer send to it.
- **Richer tray icon** (0.2.0): the tray menu lists each connected salon
  with its online players; clicking a salon opens it.
- **Discreet chat** (0.2.0): a button hides the chat panel, leaving the
  player list and link quality as the main screen; a counter on the button
  shows the messages missed meanwhile. The chat duplicates what players
  already have (Discord and the like), so it should not grow: no history,
  no voice, no file transfer. Its link stays, because it carries live
  names and avatars, images, the "salon changed" notice, mentions, and the
  list of known members who receive the new key after a ban.

## Considered and postponed

- **More than 16 players per salon.** A salon has 16 slots in the DHT; each
  player keeps one and everybody reads all of them. Two limits:
  - discovery: 32 or 64 slots mean two or four times the DHT reads, hence
    slower joins. Acceptable up to about 32 now that Nostr does the fast
    part; beyond that the scheme must change (the DHT only finds a few
    members, who then give the full list);
  - the mesh: every player punches a path to every other one and pings
    each of them every second. At 32 players that is 31 links per player,
    which modest routers start to feel.

  Going to 32 would break compatibility again (older versions would not
  see slots 17 to 32), and few LAN games go past 8 to 16 players.
  Postponed.
- **Full slots.** When the 16 slots are taken, a newcomer overwrites the
  slot of a member who is present. To fix together with the item above.
- **Voice chat, file sharing, signed reports and votes.** Paused. If file
  sharing is ever built, transfers must be strictly peer to peer, never
  through a relaying member.
- **Optional personal server** (rendezvous or relay). Only as an opt-in
  backup: the app must keep working with no server to maintain.

## Why the BitTorrent DHT, and not something else

The DHT alone would be a poor choice: reads take seconds, records expire
unless republished, values are limited to 1000 bytes. As a safety net it
has no equal: it is the only network of its kind that is huge, free,
account-less and practically impossible to take down.

Hence the current design: **Nostr for speed, the DHT for robustness.**

- Nostr alone: fast, but public relays are run by volunteers and close or
  start filtering without notice.
- Hyperswarm, libp2p / IPFS: much smaller networks, heavy dependencies,
  not faster in practice.
- A server of our own: the simplest and fastest, and exactly what this
  project refuses to depend on.

## Smaller technical leftovers

- Invite codes are accepted from 8 characters and hashed with a plain
  SHA-256 (postponed by choice: changing it invalidates shared codes).
- A knock reaches moderators in about 4 s and is read in about 4 more:
  writing it still reads the waiting room's slots first, and the reading
  side reads all slots from the DHT. Both could use the pushed copies.
- After a member restarts, EasyTier keeps its previous node for about
  20 s (the app hides the duplicate line meanwhile).
- `session::run` is about 700 lines long.
- No tests for `salons.rs`.
- Chat comes back about 12 seconds late after the other side restarts.
- Core messages (session events, technical log, diagnostic), the invite
  web page and part of the documentation are still French-only.
