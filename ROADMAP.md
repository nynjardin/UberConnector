# Roadmap and ideas

Nothing here is promised. It is the list of improvements considered so far,
with the reasoning behind each, so that they can be picked up later.

## Next, by order of interest

1. **No UAC prompt at each activation.** Windows asks for permission every
   time the app is switched on, because the helper needs administrator
   rights (virtual adapter, firewall, raw sockets). A scheduled task created
   once, with the user's consent, could start the helper elevated without a
   prompt afterwards. It is the most visible daily friction.
2. **Finish the Nostr fast path.** Nostr already delivers presence records
   in about a second. The waiting room (knocks), admissions and "the salon
   changed" notices still travel through the DHT and take several seconds
   to tens of seconds. They should go through Nostr too, the DHT staying
   the fallback.
3. **Network change during a session.** Going from Wi-Fi to cable, or a
   public IP change, strands the session until it is restarted: the LAN
   address is read once at start. The session should notice and rebuild
   its paths.
4. **Signed updates.** The updater checks the SHA-256 digest GitHub
   publishes, so it trusts whatever the GitHub account serves. Signing the
   zip with a key kept off GitHub, and checking it in the app, would
   protect users if the account were compromised.

## Done from this list

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

- The bridge socket is bound to the LAN address instead of 127.0.0.1
  (check that EasyTier accepts loopback first).
- Invite codes are accepted from 8 characters and hashed with a plain
  SHA-256.
- The salon's network secret is passed on easytier-core's command line.
- `session::run` is about 700 lines long.
- No tests for `salons.rs`.
- No release profile tuning (LTO, strip).
- `easytier-cli` is run every second per connected salon; every 2 or 3
  seconds would do.
- Chat comes back about 12 seconds late after the other side restarts.
- Core messages (session events, technical log, diagnostic), the invite
  web page and part of the documentation are still French-only.
