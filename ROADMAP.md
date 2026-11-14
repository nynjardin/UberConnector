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
3. **Hole punching through symmetric NATs.** Today a player behind a
   symmetric NAT only gets advice, and reaches others through a relaying
   member. HyperDHT (Holepunch) shows a way through: the symmetric side
   opens many sockets (a few hundred) towards the other side, which
   probes random ports; by the birthday effect one pair usually meets
   within seconds. It fits in `punch.rs`, with no new dependency, and
   only kicks in when one side reports a symmetric NAT. It does not work
   when both sides are symmetric (the relay stays the fallback there).
4. **Punching started at the same moment on both sides.** One failure
   (2026-10-29) came from one side probing about 9 s before the other.
   The Nostr hello already brings both sides close in time; a small
   "punch now" message with a start time, sent through Nostr, would make
   both start together, as HyperDHT does by relaying the handshake.

### "Launch the game and play" (inspired by SmartSteamEmu)

SmartSteamEmu's online mode (studied 2026-11-14, version 1.4.7) needs no
setup at all: the players with the same key (or, without a key, everybody
playing the same game) find each other as soon as the game starts. It does
it with public **UDP BitTorrent trackers** (BEP 15) and STUN, not the DHT:
`SHA1(":SmartSteamEmu:" + key or appid)` is announced with the STUN port
every 15 s, and every address returned gets the emulator's normal LAN
discovery packets, which punches the NATs as a side effect. Game timeouts
do not matter because discovery starts when the DLL loads, long before the
multiplayer menu. Measured with `examples/tracker_probe.rs`: a tracker
introduces two players in 0.2 to 0.6 s (the DHT takes 6 to 14 s), and with
the router port opened (NAT-PMP / UPnP) the two test PCs connected
directly. The following steps build on that, in this order:

5. **Trackers as a third rendezvous** (built in `uc_core::tracker`, 0.2.2).
   Measured between the two test PCs: the player arriving second finds the
   first one through the trackers in 1.9 s (Nostr: 1.6 s), the first one
   learns about it within its 3 s re-announce, and both sides have punched
   about 2 s after the second one started. Next to Nostr and the
   DHT: the salon's info_hash is derived from the salon key, and the
   addresses returned get an encrypted hello carrying our presence record,
   so they enter the usual punching path. Re-announce every 3 s during the
   first minute (trackers never push: the first one there only sees the
   next at its own announce), then rarely. Working trackers on
   2026-11-14: open.demonii.com:1337, explodie.org:6969,
   tracker-udp.gbitt.info:80, open.stealth.si:80,
   tracker.opentrackr.org:1337, tracker.qu.ax:6969,
   tracker.torrent.eu.org:451, exodus.desync.com:6969,
   tracker.internetwarriors.net:1337 (pads replies with 0.0.0.0:65535).
   Too slow to be useful: tracker.theoks.net (20 to 34 s),
   tracker.dler.org (about 140 s).
6. **Automatic salon per game.** A salon derived from the game's identity
   (exe name or hash) plus an optional key, as SSE does with the appid
   and its `OnlineKey`: without a key, everyone playing that game meets;
   with a key, only friends. Started when the game starts, through a
   small proxy DLL dropped next to the game (`dinput8.dll`,
   `version.dll`, `wsock32.dll`...) that reads a `uc.ini` and asks the
   app to join, or through a shortcut that launches the game through the
   app. The salon is up by the time the player reaches the multiplayer
   menu. Needs a decision on what "the game's identity" is, so that two
   copies of the same game (other folder, other patch) still meet.
7. **Capture the game's traffic inside the game (Winsock hooks).** What
   SSE does for the Steam API, done at the socket level for any LAN game:
   a DLL injected in the game intercepts `sendto` / `recvfrom` /
   `WSASendTo` / `WSARecvFrom`, `bind`, `connect`, plus the calls that
   reveal local addresses (`gethostname`, `getsockname`,
   `GetAdaptersInfo`), and hands the traffic to the app on a local port.
   The game sees one virtual 10.x address per player. Benefits: no
   administrator rights, no virtual adapter, and no "which adapter does
   the game broadcast on" problem, because a broadcast to
   255.255.255.255 is caught before it reaches any adapter (this would
   fix the "IP choisie" case of the game network test). Order: first
   catch only broadcasts (EasyTier keeps carrying the rest), test on
   Burnout 3; then all UDP; then TCP (connections to a virtual address go
   to a local relay of the app). Limits: anti-cheats may refuse the
   injection, 32 and 64 bit builds are needed, IPX games need a separate
   wrapper, DirectPlay over TCP/IP goes through Winsock and is covered.
   EasyTier would remain the fallback for games that refuse the DLL.
8. **Talking in game with a hotkey.** SSE has a full in-game overlay
   (`SSEOverlay.dll`, about 1 MB, hooks DX9/DX11/OpenGL). Old LAN games
   also use DirectDraw, D3D7/8 or Glide, so a real overlay is costly.
   Steps: first a global hotkey opening a small always-on-top chat window
   with a sound on new messages (works in windowed and borderless, not in
   exclusive fullscreen); maybe push-to-talk voice, more useful during a
   match and with nothing to draw (voice was paused, see below); a real
   overlay only later, carried by the DLL of item 7.

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
- Pears (Holepunch's runtime, built on Hyperswarm / HyperDHT): JavaScript
  only, apps run inside its own runtime; adopting it means a rewrite.
  Two of its ideas are kept above (items 3 and 4).
- Holochain: a framework for distributed apps with validated shared data,
  a heavy background process and bootstrap / signalling servers. Made for
  data, not for low-latency game traffic. At most relevant to the paused
  signed reports and votes, where signed DHT items plus Nostr suffice.
- iroh (n0, Rust): QUIC, hole punching, connect by public key, plus
  gossip and peer-to-peer file transfer (`iroh-blobs`). The closest to
  this codebase and worth evaluating if the home-made punching is ever
  replaced, or for file sharing. Limits: its default relays are run by
  n0 and far from some players, and it overlaps with EasyTier.
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
- In game mode, the EasyTier link comes 6 to 10 s after the NAT is
  punched (seen with 0.2.2 on 2026-11-14: punched at 1.9 s, link at
  10.6 s; 14.5 s on the other PC). `easytier-cli connector add` fails
  with "instance default (…)" and is retried every 2 s until the virtual
  adapter shows up, so the Wintun adapter creation is the likely cause
  (EasyTier logs "Failed to find matching adapter name" at start: it is
  created anew each session). Without an adapter the link comes in about
  1 s. Now the slowest step of a join.
- About 15 s after the first EasyTier link, the path shows "plus de lien"
  for 1 s, then "direct" again (seen twice on 2026-11-14).
- `session::run` is about 700 lines long.
- No tests for `salons.rs`.
- Chat comes back about 12 seconds late after the other side restarts.
- Core messages (session events, technical log, diagnostic), the invite
  web page and part of the documentation are still French-only.
