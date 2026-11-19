# Roadmap

Where the project is heading: the larger features considered, with the
reasoning behind each, so that they can be picked up later. Nothing here is
promised. Small concrete tasks (tests to run, technical leftovers) are in
[todo.md](todo.md); what each version changed is in its release notes.

## Planned, by order of interest

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
   protect users if the account were compromised. Waiting for a decision
   on where the key is kept.
3. **Custom rendezvous lists.** Let the user edit the Nostr relays and UDP
   BitTorrent trackers the app uses (today hard-coded: `nostr::RELAYS` and
   the list in `tracker`), and maybe the STUN servers, in the settings:
   add, remove, reset to the defaults. Useful when a public relay closes or
   is slow from the player's country, or to add one's own relay. Members
   only meet through a relay or tracker they share, so each list should
   keep the defaults unless removed on purpose, and the diagnostic should
   say which ones answered.

### "Launch the game and play" (inspired by SmartSteamEmu)

SmartSteamEmu's online mode (studied 2026-11-14, version 1.4.7) needs no
setup at all: the players with the same key (or, without a key, everybody
playing the same game) find each other as soon as the game starts. It
announces `SHA1(":SmartSteamEmu:" + key or appid)` on public UDP BitTorrent
trackers (BEP 15) with its STUN port, and sends its LAN discovery packets
to every address returned, which punches the NATs as a side effect.
Discovery starts when its DLL loads, long before the multiplayer menu, so
game timeouts do not matter. The trackers part is built (0.2.2, see
below); the next steps, in this order:

4. **Automatic salon per game.** A salon derived from the game's identity
   plus an optional key, as SSE does with the appid and its `OnlineKey`:
   without a key, everyone playing that game meets; with a key, only
   friends. Started when the game starts, either through a small proxy DLL
   dropped next to the game (`dinput8.dll`, `version.dll`, `wsock32.dll`...)
   that reads a `uc.ini` and asks the app to join, or through a shortcut
   that launches the game through the app. The salon is up by the time
   the player reaches the multiplayer menu. The proxy DLL is the better
   way, since item 5 needs the same DLL. Identity: by default the exe's
   product name (version info), else its file name, overridable in
   `uc.ini`; a hash of the exe would keep two patches of the same game
   apart.
5. **Capture the game's traffic inside the game (Winsock hooks).** What
   SSE does for the Steam API, done at the socket level for any LAN game:
   a DLL in the game intercepts `sendto` / `recvfrom` / `WSASendTo` /
   `WSARecvFrom`, `bind`, `connect`, plus the calls that reveal local
   addresses (`gethostname`, `getsockname`, `GetAdaptersInfo`), and hands
   the traffic to the app on a local port. The game sees one virtual 10.x
   address per player. Benefits: no administrator rights, no virtual
   adapter, and no "which adapter does the game broadcast on" problem,
   because a broadcast to 255.255.255.255 is caught before it reaches any
   adapter (this would fix the "IP choisie" case of the game network
   test). Order: first catch only broadcasts (EasyTier keeps carrying the
   rest), test on Burnout 3; then all UDP; then TCP (connections to a
   virtual address go to a local relay of the app). Limits: anti-cheats
   may refuse the DLL, 32 and 64 bit builds are needed, IPX games need a
   separate wrapper, DirectPlay over TCP/IP goes through Winsock and is
   covered. EasyTier stays the fallback for games that refuse the DLL.
6. **Talking in game with a hotkey.** SSE has a full in-game overlay
   (`SSEOverlay.dll`, about 1 MB, hooks DX9/DX11/OpenGL). Old LAN games
   also use DirectDraw, D3D7/8 or Glide, so a real overlay is costly.
   Steps: first a global hotkey opening a small always-on-top chat window
   with a sound on new messages (works in windowed and borderless, not in
   exclusive fullscreen); maybe push-to-talk voice, more useful during a
   match and with nothing to draw; a real overlay only later, carried by
   the DLL of item 5.

## Built recently

Newest first. Details are in the code comments and the release notes.

- **Hole punching through symmetric NATs** (0.2.6, not yet tested with a
  real symmetric NAT). Before, a player behind a symmetric NAT reached
  others only through a relaying member, or not at all when only two
  played. Now HyperDHT's birthday method is tried as a last resort (one
  side symmetric and the other not, no direct path after 10 s, at most 3
  tries): the symmetric side opens 256 ports towards the other, which
  probes up to 1500 ports of its public IP until one matches. Records
  carry each side's NAT type; the attempt is coordinated through Nostr.
  Both sides symmetric: relay only. Test hook `UC_TEST_NAT`; the real
  test needs a PC on a 4G phone hotspot (see todo.md).
- **Both sides punch at the same time** (0.2.6). Whenever we start
  probing a member, we ask it through Nostr to probe back at once, so the
  two NATs open during the same window (one failure on 2026-10-29 came
  from a 9 s gap). No start time is sent, since two PCs' clocks can be
  seconds apart. A member still unreachable gets a new joint window every
  minute for 10 minutes.
- **Double-click a salon to play in it** (0.2.6): connects it if needed
  and moves game mode to it. A double-click on the salon already in game
  mode does nothing.
- **Players not in game mode are shown** (0.2.6) in the player list and
  the game network test.
- **One shared virtual adapter** (0.2.5): created the first time a salon
  goes into game mode and reused, so later sessions start without the
  2 to 12 s Wintun wait.
- **One EasyTier connection per member** (0.2.4): router-opened and LAN
  addresses are only dialed when no punched path came up.
- **UDP BitTorrent trackers as a third rendezvous** (0.2.2), next to
  Nostr and the DHT: the player arriving second finds the first one in
  about 2 s, and both sides have punched about 2 s later.
- **Nostr fast path for salons** (0.2.1): admissions, knocks and salon
  changes (rename, bans, new key) are also pushed through Nostr as the
  same signed items the DHT holds. An admission arrives in 0.3 s instead
  of 20 to 60 s.
- **Network change during a session** (0.2.1): a new LAN or public
  address rebuilds the session by itself in about 2 s.
- **Security and speed** (0.2.1): network secret off EasyTier's command
  line, bridge on 127.0.0.1, fewer `easytier-cli` calls, release build
  tuning.
- **Richer tray icon and discreet chat** (0.2.0). The chat duplicates
  what players already have (Discord and the like), so it should not
  grow: no history, no voice, no file transfer. Its link stays, because
  it carries live names and avatars, images, the "salon changed" notice,
  mentions, and the key hand-over after a ban.

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
- **Full slots.** When the 16 slots are taken, a newcomer overwrites the
  slot of a member who is present. To fix together with the item above.
- **Stronger invite codes** (a minimum length and a slow hash). Changing
  it invalidates the codes already shared.
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

Hence the current design: **Nostr (and the trackers) for speed, the DHT
for robustness.**

- Nostr alone: fast, but public relays are run by volunteers and close or
  start filtering without notice.
- Hyperswarm, libp2p / IPFS: much smaller networks, heavy dependencies,
  not faster in practice.
- Pears (Holepunch's runtime, built on Hyperswarm / HyperDHT): JavaScript
  only, apps run inside its own runtime; adopting it means a rewrite. Two
  of HyperDHT's ideas were taken instead: joint punching and birthday
  punching.
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
