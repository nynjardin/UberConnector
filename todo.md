# TODO

Small concrete tasks. Larger features and the reasoning behind them are in
[ROADMAP.md](ROADMAP.md).

## Waiting for a real-world test

- [ ] **Birthday punching (0.2.6)** with a real symmetric NAT: one PC on a
  4G phone hotspot (its log must say "NAT SYMÉTRIQUE"), the other at home
  with port opening off, two players only. Look for "tentative
  « anniversaire » … réussie" in both diagnostics. Not possible for now.
- [ ] **Network change during a session (0.2.1)**: switch Wi-Fi / cable
  while connected; the session should rebuild itself in about 2 s.
- [ ] **Fixed port (0.1.4)** on a router without UPnP, with a manual port
  forward.

## Technical leftovers

- [ ] A knock reaches moderators in about 4 s and is read in about 4
  more: writing it still reads the waiting room's slots first, and the
  reading side reads all slots from the DHT. Both could use the pushed
  copies.
- [ ] In game mode, the first session after the app starts still waits
  for Wintun to create our card (2 to 12 s depending on the PC). Possible:
  create it when the app starts if a salon is in game mode.
- [ ] Chat comes back about 12 s late after the other side restarts.
- [ ] After a member restarts, EasyTier keeps its previous node for about
  20 s (the app hides the duplicate line meanwhile).
- [ ] `session::run` is about 700 lines long: split it.
- [ ] No tests for `salons.rs`.
- [ ] Core messages (session events, technical log, diagnostic), the
  invite web page and part of the documentation are still French-only.

## Known issues outside the app

- The test remote PC's Intel I218-V network card loses its link for about
  4 s when a virtual adapter is created (any VPN: Radmin, ZeroTier too).
  Since 0.2.5 this happens once per app start, not at every session.

## Done

- [x] User-configurable fixed port, for routers without UPnP (0.1.4,
  Réglages → "Port fixe").
