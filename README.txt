UberConnector - play "on LAN" over the Internet
================================================

No installation: unzip the folder anywhere. (The app's interface is in
French; the button names below are given as they appear on screen.)

1. Start UberConnector.exe.
2. "Créer" (create) a salon and send its invite to the other players, or
   "Rejoindre" (join) with an invite you received. The salon stays in your
   list: click it to reconnect. It exists until its owner deletes it.
   The copy button next to the invite code also gives a web link
   (https://uc.nynjardin.workers.dev/#CODE), clickable in Discord: it opens
   UberConnector with the invite ready (once the app has been started at
   least once from its current folder). "Rejoindre" accepts the code or the
   link.
3. Windows asks for permission when you switch the app on: it is needed to
   create the virtual network adapter, only while the app is switched on.
4. You can be connected to several salons at once (chat, player list), but
   only one is "en jeu" (in game): that one has the network adapter. Use
   "Jouer dans ce salon" (play in this salon) to move it.
5. In the game, pick your IP in the in-game salon (10.X.y.z, copy button).

Chat: one per salon. Nothing is kept: messages disappear on disconnection.

Avatars: emoji or picture (profile: click your name; salon: click the
salon's avatar if you own it). The salon pane folds with its arrow.

Moderation (the salon's "⋯" menu, or click a player):
- the owner appoints up to 4 moderators;
- owner and moderators can ban and change the invite code.
Banning changes the salon key: online members follow automatically, offline
ones will need the new invite.

Profile: click your name at the top right to change your name or avatar.
Others see it without reconnecting.

The switch at the top right turns the connection on or off. Closing the
window keeps the app in the notification area. To quit completely:
right-click the tray icon, then "Quitter".

Connecting players: nothing to configure. UberConnector asks your router to
open its port (UPnP / NAT-PMP, closed on disconnection); one of the two
players having a router that accepts is enough. Otherwise it punches through
the NATs, which works with most routers. Can be turned off in the settings.
The player list shows each link's quality (direct or relayed by another
player, ping, packet loss); click a player for details.

Updates: the app checks GitHub for a new version at start, then every 6
hours, and offers to install it (never without your consent). Your data
(uc-data) is kept. Can be turned off in the settings.

Settings (gear icon): start with Windows, updates, automatic port opening,
backup relays, Windows notifications chosen one by one (player online,
waiting room, your name mentioned in the chat). Notifications only show
while the window is not in front; clicking one opens its salon.

Diagnostic tools (double-click):
- nat_check.exe: NAT type of this network (cone or symmetric).
- punch_test.exe: run it on two PCs at the same time, each one typing the
  address shown by the other; tells whether a direct path opens between them.

Command line (advanced, administrator terminal):
  uc-helper new  --salon "Name" --name Nick [--no-lan] [--relay tcp://host:11010]
  uc-helper join CODE --name Nick [--ip 10.21.0.10]

Files: uc-data\ folder (settings, salons, uc-helper.log, logs\).
IMPORTANT: identity.json holds your personal key. If you lose it, you lose
the ownership of your salons. Back it up, and never share it.
