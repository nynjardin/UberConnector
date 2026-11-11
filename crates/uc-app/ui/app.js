const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const store = {
  get: (k) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k, v) => { try { localStorage.setItem(k, v); } catch {} },
};

let state = null;
// Salon shown in the middle (UI only, remembered locally).
let selected = store.get("selected");

const PHASE_TEXT = { off: "Désactivé", starting: "Autorisation…", on: "Activé" };
const EMOJIS = ["🎮", "🕹️", "👾", "🤖", "🏎️", "🚀", "⚡", "🔥", "💀", "👻",
                "🦊", "🐺", "🐱", "🐼", "🐸", "🐙", "🦉", "🐲", "🦄", "🐧",
                "🍕", "🌵", "🎧", "🎲", "⚽", "🏆", "🛡️", "⚔️", "🧙", "🥷"];
const COLORS = ["#38d6a0", "#4b8df2", "#9b6cf2", "#f26c9b", "#f2b84b", "#f07b4b", "#5cc8d6", "#8a94a8"];
const DEFAULT_AVATAR = "🎮|#38d6a0";

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function toast(msg) {
  const box = $("toast");
  box.textContent = msg;
  box.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (box.hidden = true), 1800);
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = el("textarea");
    ta.value = text;
    document.body.append(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  toast(t("Copié : {text}", { text }));
}

// ---- Images (custom avatars), fetched from the local cache by hash ----
const images = {}; // hash -> data URL | { retryAt }
let repaintTimer = null;

function imageUrl(hash) {
  const v = images[hash];
  if (typeof v === "string") return v;
  if (v && Date.now() < v.retryAt) return null;
  images[hash] = { retryAt: Date.now() + 8000 };
  invoke("get_image", { hash }).then((url) => {
    if (url) {
      images[hash] = url;
      repaint();
    }
  });
  return null;
}

// Re-renders once an image arrives (debounced).
function repaint() {
  clearTimeout(repaintTimer);
  repaintTimer = setTimeout(() => {
    if (state) render(state);
    renderChat();
  }, 50);
}

listen("image", ({ payload: hash }) => {
  delete images[hash];
  repaint();
});

// ---- Avatars: "emoji|#color", "img:<hash>", or "" (initials for salons) ----
function parseAvatar(a) {
  if (a && a.startsWith("img:")) return { image: a.slice(4) };
  const [emoji, color] = (a || DEFAULT_AVATAR).split("|");
  // Avatars come from other players: only accept a plain hex color.
  return { emoji: emoji || "🎮", color: /^#[0-9a-fA-F]{6}$/.test(color || "") ? color : "#8a94a8" };
}

function hashColor(s) {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return COLORS[h % COLORS.length];
}

function initials(name) {
  const words = name.trim().split(/\s+/).filter(Boolean);
  return ((words[0]?.[0] || "?") + (words[1]?.[0] || "")).toUpperCase();
}

function paintAvatar(node, a, fallbackName) {
  node.replaceChildren();
  node.classList.remove("initials");
  node.style.background = "";
  node.style.boxShadow = "";
  if (!a && fallbackName !== undefined) {
    node.classList.add("initials");
    node.textContent = initials(fallbackName);
    node.style.background = hashColor(fallbackName);
    return node;
  }
  const p = parseAvatar(a);
  if (p.image) {
    const url = imageUrl(p.image);
    if (url) {
      const img = el("img");
      img.src = url;
      img.alt = "";
      node.append(img);
    } else {
      node.textContent = "…";
    }
    return node;
  }
  node.textContent = p.emoji;
  node.style.background = p.color + "33";
  node.style.boxShadow = `inset 0 0 0 1.5px ${p.color}`;
  return node;
}

function avatarEl(a, cls = "", fallbackName) {
  return paintAvatar(el("span", "avatar " + cls), a, fallbackName);
}

// ---- Rendering ----
const lossOf = (m) => parseFloat(m.loss) || 0;

// Link quality from latency and packet loss ("" while unknown).
function quality(m) {
  const ms = parseFloat(m.latency_ms);
  if (Number.isNaN(ms)) return "";
  const loss = lossOf(m);
  if (ms >= 150 || loss >= 5) return "bad";
  if (ms >= 60 || loss >= 1) return "mid";
  return "good";
}

// `short`: for the narrow player list (details are in the player's menu).
function pathLabel(m, short = false) {
  if (m.path.startsWith("direct")) return m.path.includes("NAT percé") && !short ? t("direct (NAT percé)") : "direct";
  if (m.path.startsWith("relais")) return m.relay_via ? t("relayé par {name}", { name: m.relay_via }) : t("relayé");
  return m.path;
}

// Details and advice shown in a player's menu.
function linkNotes(m) {
  const ping = m.latency_ms === "-" ? "–" : Math.round(parseFloat(m.latency_ms)) + " ms";
  const out = [{ title: t("{path} · ping {ping} · perte {loss} %", { path: pathLabel(m), ping, loss: lossOf(m) }) }];
  if (m.path.startsWith("relais")) {
    const via = m.relay_via ? t("Le trafic passe par {name}", { name: m.relay_via }) : t("Le trafic passe par un autre joueur");
    out.push({ note: t("{via} : un peu plus de latence, et la connexion tombe s'il part. Une connexion directe est possible si l'un de vous deux a une box qui ouvre les ports (UPnP).", { via }), warn: true });
  }
  if (lossOf(m) >= 5) out.push({ note: t("Beaucoup de paquets perdus : saccades probables. Wi-Fi faible ou connexion saturée chez l'un de vous ?"), warn: true });
  else if (parseFloat(m.latency_ms) >= 150) out.push({ note: t("Ping élevé : les jeux rapides peuvent en souffrir."), warn: true });
  return out;
}

// Debug: live ping graph of the last minute, then the 5-minute figures.
function renderPing(node, p) {
  if (!p) {
    node.replaceChildren(el("div", "menu-note", t("Ping mesuré : en attente (mode jeu, quelques secondes après la connexion).")));
    return;
  }
  const W = 240, H = 64;
  const recent = p.recent;
  const got = recent.filter((v) => v != null);
  const top = Math.max(20, Math.ceil(Math.max(0, ...got) * 1.15));
  // Newest on the right edge, one step per second.
  const x = (i) => W - (recent.length - 1 - i) * (W / 59);
  const y = (v) => H - 2 - (v / top) * (H - 4);
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.classList.add("ping-chart");
  const add = (tag, attrs) => {
    const e = document.createElementNS(ns, tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
    svg.append(e);
  };
  for (let i = 1; i < 4; i++) add("line", { class: "grid", x1: 0, x2: W, y1: (H * i) / 4, y2: (H * i) / 4 });
  for (let i = 1; i < 6; i++) add("line", { class: "grid", y1: 0, y2: H, x1: (W * i) / 6, x2: (W * i) / 6 });
  let d = "";
  recent.forEach((v, i) => {
    if (v == null) {
      add("line", { class: "lost", x1: x(i), x2: x(i), y1: 0, y2: H });
      d += " ";
    } else d += `${d.endsWith(" ") || !d ? "M" : "L"}${x(i).toFixed(1)},${y(v).toFixed(1)}`;
  });
  add("path", { class: "line", d: d.trim() });
  const box = el("div", "ping-box");
  box.append(svg, el("span", "ping-top", `${top} ms`), el("span", "ping-span", "60 s"));
  // The warning looks at the last minute only, so it clears once the link is steady.
  const jit = got.length > 1 ? got.slice(1).reduce((a, v, i) => a + Math.abs(v - got[i]), 0) / (got.length - 1) : 0;
  const med = [...got].sort((a, b) => a - b)[Math.floor(got.length / 2)] ?? 0;
  const spikes = got.filter((v) => v >= med + 20).length;
  const lost = recent.length - got.length;
  const out = [
    box,
    el("div", "menu-note", t("Dernière minute : {range} · gigue {jit} ms · perdus {lost}", {
      range: got.length ? t("{min}–{max} ms, médiane {med}", { min: Math.min(...got), max: Math.max(...got), med }) : t("aucune réponse"),
      jit: jit.toFixed(1),
      lost,
    })),
    el("div", "menu-note", t("5 min : {min}–{max} ms, médiane {med} · gigue {jit} ms · pics {spikes} · perdus {lost}/{sent}",
      { min: p.min, max: p.max, med: p.median, jit: p.jitter.toFixed(1), spikes: p.spikes, lost: p.lost, sent: p.sent })),
  ];
  if (got.length >= 10 && (jit >= 10 || spikes * 20 >= got.length)) {
    out.push(el("div", "menu-note warn", t("Ping instable : les paquets arrivent tous, mais à des délais variables (Wi-Fi, connexion chargée par un téléchargement ou un stream…).")));
  }
  node.replaceChildren(...out);
}

// How others reach us, with advice when it is fragile.
function myNetText(st) {
  if (!st) return null;
  if (st.port_open) return { cls: "good", text: t("✓ Joignable directement : port ouvert par votre box ({how}).", { how: st.port_open }) };
  const off = state.settings.port_map ? "" : t(" (ouverture automatique désactivée dans Réglages)");
  if (st.nat === "symmetric") {
    return { cls: "bad", text: t("Votre box ne permet ni d'ouvrir un port{off} ni de percer les NAT : les connexions directes échoueront souvent, un autre joueur devra vous relayer. Activez l'UPnP dans l'interface de votre box.", { off }) };
  }
  // Usually fine: informative only, no instructions.
  if (st.nat === "cone") return { cls: "", text: t("Connexions directes par perçage des NAT (pas de port ouvert sur votre box{off}).", { off }) };
  return null;
}

function select(id) {
  selected = id;
  delete unread[id];
  store.set("selected", id ?? "");
  if (state) render(state);
}

function render(s) {
  state = s;
  refreshMenu();
  pruneChats(s);
  const connected = new Set(s.settings.connected);
  if (!s.salons.some((x) => x.id === selected)) {
    selected = s.settings.connected[0] ?? s.salons[0]?.id ?? null;
  }
  const cur = s.salons.find((x) => x.id === selected);
  const st = cur ? s.statuses[cur.id] : null;
  const isOn = !!cur && connected.has(cur.id);
  const live = isOn && s.phase === "on";

  // Header
  paintAvatar($("me-avatar"), s.settings.avatar);
  $("me-name").textContent = s.settings.name;
  const pill = $("phase-pill");
  pill.dataset.phase = s.phase;
  const n = connected.size;
  pill.textContent = s.phase === "on" && n ? t("Connecté · {n} salon{s}", { n, s: n > 1 ? "s" : "" }) : t(PHASE_TEXT[s.phase]);
  const sw = $("main-switch");
  sw.checked = s.phase !== "off";
  sw.disabled = s.phase === "starting";
  $("busy").hidden = !s.busy;
  $("busy").textContent = s.busy || "";
  $("banner").hidden = !s.error;
  $("banner").textContent = s.error || "";

  // Salons list
  $("salons").replaceChildren(
    ...s.salons.map((x) => {
      const b = el("button", "salon");
      if (x.id === selected) b.classList.add("selected");
      if (x.deleted) b.classList.add("deleted");
      b.title = x.name;
      const icon = el("span", "icon");
      icon.append(avatarEl(x.avatar, "lg square", x.name));
      if (connected.has(x.id) && s.phase === "on") icon.append(el("span", "dot"));
      b.append(icon, el("span", "name", x.name));
      if (x.id === s.settings.game_salon && s.phase === "on") b.append(el("span", "badge game", t("actif")));
      if (unread[x.id] && x.id !== selected) b.append(el("span", "unread", String(unread[x.id])));
      if (x.deleted) b.append(el("span", "badge", t("supprimé")));
      if (x.pending) b.append(el("span", "badge wait", t("en attente")));
      if (x.knocks.length) b.append(el("span", "unread", String(x.knocks.length)));
      b.onclick = () => select(x.id);
      const li = el("li");
      li.append(b);
      return li;
    })
  );
  $("salons-empty").hidden = s.salons.length > 0;

  // Selected salon
  $("current").hidden = !cur;
  $("current-none").hidden = !!cur;
  // Bottom bar: waiting room until admitted, then game / chat-only mode.
  $("game-bar").hidden = !live && !cur?.pending;
  if (!cur) return;

  const av = $("cur-avatar");
  av.replaceChildren(avatarEl(cur.avatar, "lg square", cur.name));
  av.classList.toggle("editable", cur.is_owner && !cur.deleted);
  av.title = cur.is_owner ? t("Changer l'avatar du salon") : "";
  $("cur-name").textContent = cur.name;
  $("cur-name").title = cur.name;
  $("cur-invite").textContent = cur.deleted ? "—" : cur.invite;
  $("key-lost").hidden = !cur.key_lost;
  const btn = $("btn-connect");
  btn.hidden = cur.deleted || cur.key_lost || cur.pending;
  btn.textContent = isOn ? t("Déconnecter") : t("Se connecter");
  btn.classList.toggle("primary", !isOn);

  // Bottom bar: the single game salon has the virtual adapter.
  const game = live && s.settings.game_salon === cur.id;
  $("game-bar").classList.toggle("game", game);
  $("game-bar").classList.toggle("waiting", cur.pending);
  $("mode-spinner").hidden = !cur.pending;
  $("btn-game").hidden = cur.pending;
  $("mode-title").textContent = t(cur.pending ? "Salle d'attente" : game ? "En jeu" : "Chat seulement");
  $("mode-desc").textContent = cur.pending
    ? t("Le propriétaire ou un modérateur doit valider votre arrivée. Laissez UberConnector ouvert : la connexion se fera toute seule.")
    : game
    ? t("Carte réseau active, plage 10.{subnet}.0.0/16 : choisissez votre IP dans le jeu.", { subnet: cur.subnet })
    : t("Pas de carte réseau pour ce salon : chat et liste des joueurs uniquement.");
  $("mode-ip-box").hidden = !game;
  $("btn-gametest").hidden = !game;
  $("cur-ip").textContent = st?.my_vip || "–";
  $("btn-game").textContent = game ? t("Quitter le mode jeu") : t("Jouer dans ce salon");
  $("btn-game").classList.toggle("primary", !game);
  $("btn-game").title = game ? "" : t("Déplace la carte réseau vers ce salon (un seul salon en jeu à la fois)");

  // Players
  const members = st?.members || [];
  $("players-wait").hidden = members.length > 0;
  $("players-wait").textContent = t(
    cur.deleted ? "Ce salon a été supprimé par son propriétaire."
    : cur.pending ? "Les joueurs apparaîtront une fois votre arrivée validée."
    : !isOn ? "Vous n'êtes pas connecté à ce salon."
    : s.phase === "starting" ? "Acceptez la demande d'autorisation de Windows…"
    : s.phase === "off" ? "UberConnector est désactivé."
    : "Connexion au salon…");
  $("count").textContent = members.length ? `(${members.length})` : "";
  const net = live ? myNetText(st) : null;
  $("my-net").hidden = !net;
  $("my-net").className = "my-net small " + (net?.cls || "");
  $("my-net").textContent = net?.text || "";
  $("players").replaceChildren(
    ...members.map((m) => {
      const me = m.path === "vous";
      const li = el("li", "player");
      li.title = me ? t("{name} (vous) · {vip}", { name: m.name, vip: m.vip })
        : t("{name}\n{vip} · {path} · perte {loss} % · NAT {nat}\nCliquer pour les détails", { name: m.name, vip: m.vip, path: pathLabel(m), loss: lossOf(m), nat: m.nat });
      li.onclick = (e) => playerMenu(e, cur, m, me);
      const ping = me
        ? el("span")
        : el("span", "ping " + quality(m), m.latency_ms === "-" ? "–" : `${Math.round(parseFloat(m.latency_ms))} ms`);
      const sub = el("span", "player-sub", me ? t("vous · {vip}", { vip: m.vip }) : `${m.vip} · ${pathLabel(m, true)}`);
      if (!me && !m.chat) sub.prepend(el("span", "waiting", "chat… · "));
      if (!me && lossOf(m) >= 1) sub.append(el("span", lossOf(m) >= 5 ? "loss bad" : "loss", t(" · perte {loss} %", { loss: lossOf(m) })));
      const main = el("span", "player-main", m.name);
      const role = roleOf(cur, m.id);
      if (role) main.append(el("span", "role", role.icon));
      if (role) main.title = role.label;
      if (!me) main.append(...flagEls(cur, m.id));
      li.append(avatarEl(m.avatar), main, ping, sub);
      return li;
    })
  );

  // Waiting room (owner and moderators of a gated salon).
  $("knocks-box").hidden = !cur.knocks.length;
  $("knocks-count").textContent = `(${cur.knocks.length})`;
  $("knocks").replaceChildren(...cur.knocks.map((k) => knockEl(cur, k)));

  // Chat link state: messages are kept and sent once a member is linked.
  const others = members.filter((m) => m.path !== "vous");
  const linked = others.filter((m) => m.chat).length;
  // What the user waits for, from session start to every member linked.
  const KEEP = t(" Vous pouvez déjà écrire : vos messages seront envoyés dès que possible.");
  const stage = st?.stage || "dht";
  let pendingText = null;
  let spinning = true;
  if (live) {
    if (others.length > 0 && linked === 0) pendingText = t("Connexion du chat aux autres joueurs…") + KEEP;
    else if (others.length > 0 && linked < others.length) pendingText = t("Chat relié à {linked} joueur{s} sur {total}, connexion des autres en cours…", { linked, s: linked > 1 ? "s" : "", total: others.length });
    else if (others.length === 0 && stage === "dht") pendingText = t("Connexion au réseau de découverte…") + KEEP;
    else if (others.length === 0 && stage === "starting") pendingText = t("Démarrage du réseau du salon…") + KEEP;
    else if (others.length === 0 && stage === "searching") pendingText = t("Recherche des autres joueurs du salon (jusqu'à une minute)…") + KEEP;
    else if (others.length === 0) {
      pendingText = t("Aucun autre joueur en ligne pour l'instant. Ils apparaîtront ici dès leur connexion.");
      spinning = false;
    }
  }
  $("chat-pending").hidden = !pendingText;
  $("chat-pending").classList.toggle("idle", !spinning);
  $("chat-pending-text").textContent = pendingText || "";

  // Chat
  const canChat = live && !cur.deleted;
  $("chat-input").disabled = !canChat;
  $("chat-form").querySelector("button").disabled = !canChat;
  $("chat-input").placeholder = canChat ? t("Écrire dans {name}…", { name: cur.name })
    : t(cur.pending ? "En attente de validation…" : "Connectez-vous au salon pour discuter");
  if (renderedChat !== cur.id) renderChat();
  if ($("log").open) renderLog();
  if ($("gametest").open) renderGameTest();
  if ($("bans").open) renderBans();
}

function flagEls(salon, id) {
  const out = [];
  const warn = warningsOf(salon, id);
  if (warn.bannedIn.length) {
    const b = el("span", "flag bad", "⛔");
    b.title = t("Banni de : {list}", { list: warn.bannedIn.map((x) => x.name).join(", ") });
    out.push(b);
  }
  if (warn.noob) {
    const b = el("span", "flag noob", "Noob");
    b.title = t("Identité rencontrée pour la première fois il y a moins de 3 jours");
    out.push(b);
  }
  return out;
}

function knockEl(salon, k) {
  const li = el("li", "player knock");
  const main = el("span", "player-main", k.name);
  main.append(...flagEls(salon, k.id));
  const warn = warningsOf(salon, k.id);
  li.title = warn.bannedIn.length ? `${k.name}\n` + t("Banni de : {list}", { list: warn.bannedIn.map((x) => x.name).join(", ") }) : k.name;
  const actions = el("span", "knock-actions");
  const ok = el("button", "btn small primary", t("Accepter"));
  const no = el("button", "btn small", t("Ignorer"));
  ok.onclick = async () => {
    ok.disabled = no.disabled = true;
    ok.textContent = t("Validation…");
    try {
      await invoke("admit_member", { id: salon.id, member: k.id });
      toast(t("{name} est accepté : la connexion se fait de son côté d'ici une minute", { name: k.name }));
    } catch (err) {
      toast(String(err));
      ok.disabled = no.disabled = false;
      ok.textContent = t("Accepter");
    }
  };
  no.onclick = () => invoke("dismiss_knock", { id: salon.id, member: k.id });
  actions.append(ok, no);
  li.append(avatarEl(k.avatar, "", k.name), main, el("span"), actions);
  return li;
}

// Anti-abuse hints from our other salons (bans) and first meeting (noob).
function warningsOf(salon, id) {
  const p = (id && state.people[id]) || { banned_in: [], noob: false };
  return { bannedIn: p.banned_in.filter((x) => x.id !== salon.id), noob: p.noob };
}

function roleOf(salon, id) {
  if (!id) return null;
  if (id === salon.owner_id) return { icon: "👑", label: t("Propriétaire") };
  if (salon.mod_ids.includes(id)) return { icon: "🛡️", label: t("Modérateur") };
  return null;
}

// ---- Game network test ----
function renderGameTest() {
  const cur = current();
  if (!cur) return;
  $("gt-salon").textContent = `· ${cur.name}`;
  const test = state.statuses[cur.id]?.game_test;
  const running = !test || test.running;
  const rows = test?.results || [];
  const cell = (ok, text) => el("td", ok ? "ok" : running ? "wait" : "ko", ok ? text : running ? "…" : "✗");
  $("gt-rows").replaceChildren(
    ...rows.map((r) => {
      const tr = el("tr");
      tr.append(
        el("td", "", r.name || r.id),
        cell(r.direct_ms != null, `✓ ${r.direct_ms} ms`),
        cell(r.subnet, "✓"),
        cell(r.global_bound, "✓"),
        cell(r.global, "✓"),
      );
      return tr;
    })
  );
  $("gt-empty").hidden = running || rows.length > 0 || !!test?.error;
  $("gt-running").hidden = !running;
  $("gt-again").disabled = running;
  let hint = test?.error ? t("Test impossible : {err}.", { err: test.error }) : "";
  if (!running && rows.length) {
    const none = rows.filter((r) => r.direct_ms == null && !r.subnet && !r.global && !r.global_bound);
    const noBound = rows.filter((r) => r.direct_ms != null && !r.global_bound);
    const noAuto = rows.filter((r) => r.direct_ms != null && !r.global);
    const rivals = test.rivals?.length ? t(" Sur ce PC, Windows les envoie sans doute par une autre carte réseau : {list}. Désactivez-la pendant la partie pour que ces jeux voient les autres.", { list: test.rivals.join(", ") }) : "";
    if (none.length) hint = t("Sans réponse : {list}. Ces joueurs ne sont peut-être pas en jeu dans ce salon (« Chat seulement »), ou leur pare-feu bloque le jeu.", { list: none.map((r) => r.name || r.id).join(", ") });
    else if (noBound.length && noAuto.length) hint = t("Les parties rejointes par IP fonctionneront, mais les annonces de parties ne passent pas : saisissez l'IP de l'hôte dans le jeu.");
    else if (noAuto.length) hint = t("Les jeux où l'on choisit son IP (comme Burnout) verront les parties. Ceux qui ne laissent pas choisir risquent de ne pas les voir.") + rivals;
    else if (noBound.length) hint = t("Les jeux qui ne laissent pas choisir l'IP verront les parties. Dans ceux où l'on choisit son IP, rejoignez l'hôte par son IP si sa partie n'apparaît pas.");
    else hint = t("Tout est bon : le jeu verra les autres joueurs comme sur un réseau local.");
  }
  if (test?.relayed != null) hint += t(" (Annonces de jeux relayées depuis ce PC : {n}.)", { n: test.relayed });
  $("gt-hint").textContent = hint;
}
function startGameTest() {
  const cur = current();
  invoke("game_test", { id: cur.id });
  // Until the first status with the new test arrives.
  if (state.statuses[cur.id]) state.statuses[cur.id].game_test = { running: true, results: [] };
  renderGameTest();
}
$("btn-gametest").addEventListener("click", () => { startGameTest(); $("gametest").showModal(); });
$("gt-again").addEventListener("click", startGameTest);
$("gt-close").addEventListener("click", () => $("gametest").close());

// ---- Popup menu ----
const menu = $("menu");
// Menu parts redrawn on each state update while the menu is open.
let menuLive = [];
function refreshMenu() {
  if (!menu.hidden) for (const f of menuLive) f();
}
function showMenu(event, items) {
  menuLive = [];
  menu.replaceChildren(
    ...items.map((it) => {
      if (it === "sep") return el("hr");
      if (it.title) return el("div", "menu-title", it.title);
      if (it.note) return el("div", "menu-note" + (it.warn ? " warn" : ""), it.note);
      if (it.ping) {
        const node = el("div", "menu-ping");
        renderPing(node, it.ping());
        menuLive.push(() => renderPing(node, it.ping()));
        return node;
      }
      const b = el("button", it.danger ? "danger" : "", it.label);
      b.onclick = () => { hideMenu(); it.run(); };
      return b;
    })
  );
  menu.hidden = false;
  const r = menu.getBoundingClientRect();
  menu.style.left = Math.min(event.clientX, innerWidth - r.width - 8) + "px";
  menu.style.top = Math.min(event.clientY, innerHeight - r.height - 8) + "px";
  event.stopPropagation();
}
function hideMenu() { menu.hidden = true; }
document.addEventListener("click", (e) => { if (!menu.contains(e.target)) hideMenu(); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") hideMenu(); });

function playerMenu(event, salon, m, me) {
  const items = [{ title: m.name }];
  if (!me) {
    const warn = warningsOf(salon, m.id);
    for (const x of warn.bannedIn) items.push({ title: t("⛔ Banni de « {name} »", { name: x.name }) });
    if (warn.noob) items.push({ title: t("Noob : rencontré il y a moins de 3 jours") });
    items.push(...linkNotes(m));
    if (state.settings.debug) {
      items.push({ ping: () => state.statuses[salon.id]?.members.find((x) => x.id === m.id)?.ping });
    }
  }
  items.push({ label: t("Copier l'IP ({vip})", { vip: m.vip }), run: () => copy(m.vip) });
  const isOwner = m.id === salon.owner_id;
  const isMod = salon.mod_ids.includes(m.id);
  if (!me && m.id && !salon.deleted) {
    if (salon.is_owner) {
      items.push(isMod
        ? { label: t("Retirer le rôle de modérateur"), run: () => toast(t("Mise à jour des modérateurs…")) || act(invoke("set_moderator", { id: salon.id, member: m.id, on: false }), t("Modérateur retiré")) }
        : { label: t("Nommer modérateur"), run: () => toast(t("Nomination en cours, jusqu'à 30 secondes…")) || act(invoke("set_moderator", { id: salon.id, member: m.id, on: true }), t("{name} est modérateur", { name: m.name })) });
    }
    if (salon.can_moderate && !isOwner && (salon.is_owner || !isMod)) {
      items.push("sep", { label: t("Bannir…"), danger: true, run: () => confirmBan(salon, m) });
    }
  }
  showMenu(event, items);
}

function confirmBan(salon, m) {
  ask({
    title: t("Bannir {name} ?", { name: m.name }),
    text: t("Il sera exclu du chat immédiatement. La clé du salon et le code d'invitation vont changer : les membres connectés suivront automatiquement, les autres auront besoin du nouveau code."),
    ok: t("Bannir"),
    danger: true,
    busy: t("Bannissement : nouvelle clé et nouveau code distribués aux membres, cela peut prendre jusqu'à une minute…"),
    run: async () => {
      const n = await invoke("ban_member", { id: salon.id, member: m.id });
      toast(t("{name} est banni · nouvelle clé remise à {n} membre{s}", { name: m.name, n, s: n > 1 ? "s" : "" }));
    },
  });
}

async function act(promise, okMsg) {
  try {
    await promise;
    if (okMsg) toast(okMsg);
  } catch (err) {
    toast(String(err));
  }
}

$("btn-manage").addEventListener("click", (e) => {
  const cur = current();
  const items = [{ title: cur.name }];
  if (cur.is_owner && !cur.deleted) {
    items.push(
      { label: t("Changer l'avatar du salon"), run: () => $("cur-avatar").click() },
      { label: t("Renommer…"), run: renameSalon },
      { label: t(cur.gated ? "✓ Validation des nouveaux venus…" : "Validation des nouveaux venus…"), run: () => setGated(cur) },
    );
  }
  if (cur.can_moderate && !cur.deleted) {
    items.push(
      { label: t("Nouveau code d'invitation…"), run: () => regenerate(cur) },
      { label: t("Membres bannis ({n})", { n: cur.banned.length }), run: openBans },
    );
  }
  items.push("sep");
  if (cur.is_owner && !cur.deleted) items.push({ label: t("Supprimer le salon…"), danger: true, run: deleteSalon });
  else items.push({ label: t("Retirer de ma liste…"), danger: true, run: forgetSalon });
  showMenu(e, items);
});

function setGated(cur) {
  if (cur.gated) {
    ask({
      title: t("Désactiver la validation ?"),
      text: t("Toute personne ayant le code d'invitation pourra entrer et jouer directement. Les personnes en salle d'attente entreront aussi."),
      ok: t("Ouvrir le salon"),
      busy: t("Publication du changement…"),
      run: async () => {
        await invoke("set_gated", { id: cur.id, on: false });
        toast(t("Salon ouvert : le code suffit pour entrer"));
      },
    });
    return;
  }
  ask({
    title: t("Valider les nouveaux venus ?"),
    text: t("Le code d'invitation ne donnera plus accès au salon : chaque nouveau venu attendra qu'un modérateur ou vous l'acceptiez. La clé du salon change maintenant et les membres connectés la reçoivent automatiquement. Les membres hors ligne repasseront par la salle d'attente."),
    ok: t("Activer"),
    busy: t("Activation : nouvelle clé distribuée aux membres, cela peut prendre jusqu'à une minute…"),
    run: async () => {
      const n = await invoke("set_gated", { id: cur.id, on: true });
      toast(t("Validation activée · nouvelle clé remise à {n} membre{s}", { n, s: n > 1 ? "s" : "" }));
    },
  });
}

function regenerate(cur) {
  ask({
    title: t("Nouveau code d'invitation ?"),
    text: t("L'ancien code ne fonctionnera plus et la clé du salon change. Les membres connectés suivent automatiquement, les autres auront besoin du nouveau code."),
    ok: t("Changer le code"),
    busy: t("Nouvelle clé et nouveau code distribués aux membres, cela peut prendre jusqu'à une minute…"),
    run: async () => {
      const n = await invoke("regenerate_invite", { id: cur.id });
      toast(t("Nouveau code · clé remise à {n} membre{s}", { n, s: n > 1 ? "s" : "" }));
    },
  });
}

// ---- Bans ----
function renderBans() {
  const cur = current();
  if (!cur) return;
  $("bans-list").replaceChildren(
    ...cur.banned.map((b) => {
      const li = el("li");
      li.append(el("span", "who", b.name ? `${b.name} (${b.id})` : b.id));
      const btn = el("button", "btn small", t("Débannir"));
      btn.onclick = async () => {
        $("bans-error").hidden = true;
        $("bans-busy").hidden = false;
        $("bans-list").querySelectorAll("button").forEach((x) => (x.disabled = true));
        try {
          await invoke("unban_member", { id: cur.id, member: b.id });
          toast(t("Bannissement levé"));
        } catch (err) {
          $("bans-error").textContent = String(err);
          $("bans-error").hidden = false;
        } finally {
          $("bans-busy").hidden = true;
          renderBans();
        }
      };
      li.append(btn);
      return li;
    })
  );
  $("bans-empty").hidden = cur.banned.length > 0;
}
function openBans() {
  $("bans-error").hidden = true;
  renderBans();
  $("bans").showModal();
}
$("bans-close").addEventListener("click", () => $("bans").close());

// ---- Chat (memory only: cleared on disconnect) ----
const chats = {};
const unread = {};
let renderedChat = null;

function fmtTime(ts) {
  return new Date(ts * 1000).toLocaleTimeString(LANG === "fr" ? "fr-FR" : undefined, { hour: "2-digit", minute: "2-digit" });
}

function lineEl(line, prev) {
  if (line.system) return el("li", "sys", line.text);
  // Consecutive messages of the same sender within 2 minutes are grouped.
  const cont = prev && !prev.system && prev.from === line.from && line.ts - prev.ts < 120;
  const li = el("li", "msg" + (line.mine ? " mine" : "") + (cont ? " cont" : ""));
  const head = el("div", "msg-head");
  head.append(el("span", "msg-name", line.name || "?"), el("span", "msg-time", fmtTime(line.ts)));
  li.append(avatarEl(line.avatar), head, el("div", "msg-text", line.text));
  return li;
}

function renderChat() {
  if (!selected) return;
  const log = $("chat-log");
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
  const switched = renderedChat !== selected;
  renderedChat = selected;
  const lines = chats[selected] || [];
  log.replaceChildren(...lines.map((l, i) => lineEl(l, lines[i - 1])));
  log.hidden = lines.length === 0;
  $("chat-empty").hidden = lines.length > 0;
  $("chat-empty").textContent = t("Aucun message. Les messages ne sont pas conservés : ils disparaissent à la déconnexion.");
  if (switched || atBottom) log.scrollTop = log.scrollHeight;
}

listen("chat", ({ payload: { salon_id, line } }) => {
  const list = (chats[salon_id] ||= []);
  const prev = list[list.length - 1];
  list.push(line);
  if (list.length > 500) list.splice(0, list.length - 500);
  if (salon_id === renderedChat) {
    const log = $("chat-log");
    const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
    $("chat-empty").hidden = true;
    log.hidden = false;
    log.append(lineEl(line, prev));
    if (atBottom || line.mine) log.scrollTop = log.scrollHeight;
  } else if (!line.system) {
    unread[salon_id] = (unread[salon_id] || 0) + 1;
    if (state) render(state);
  }
});

// Drops chats of salons we are no longer connected to.
function pruneChats(s) {
  for (const id of Object.keys(chats)) {
    if (s.phase !== "on" || !s.settings.connected.includes(id)) {
      delete chats[id];
      delete unread[id];
      if (id === renderedChat) renderedChat = null;
    }
  }
}

$("chat-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const input = $("chat-input");
  const text = input.value.trim();
  if (!text || !selected) return;
  try {
    await invoke("say", { id: selected, text });
    input.value = "";
  } catch (err) {
    toast(String(err));
  }
});

// ---- Generic prompt / confirm dialog. `input` (optional) = initial value ----
const askDlg = $("ask");
let askRun = null;
// `busy`: text shown while `run` is in progress (DHT operations are slow).
let askBusy = "";
function ask({ title, text = "", input, placeholder = "", ok = "OK", danger = false, busy = "", run }) {
  askBusy = busy;
  $("ask-busy").hidden = true;
  $("ask-input").disabled = false;
  $("ask-cancel").textContent = t("Annuler");
  $("ask-title").textContent = title;
  $("ask-text").textContent = text;
  const inp = $("ask-input");
  inp.hidden = input === undefined;
  inp.value = input ?? "";
  inp.placeholder = placeholder;
  $("ask-ok").textContent = ok;
  $("ask-ok").classList.toggle("danger", danger);
  $("ask-error").hidden = true;
  askRun = run;
  askDlg.showModal();
  if (!inp.hidden) inp.focus();
}
$("ask-cancel").addEventListener("click", () => askDlg.close());
$("ask-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const okBtn = $("ask-ok");
  okBtn.disabled = true;
  $("ask-error").hidden = true;
  if (askBusy) {
    $("ask-busy-text").textContent = askBusy;
    $("ask-busy").hidden = false;
    $("ask-input").disabled = true;
    // Closing does not cancel: the operation goes on in the background.
    $("ask-cancel").textContent = t("Fermer");
  }
  try {
    await askRun($("ask-input").value.trim());
    askDlg.close();
  } catch (err) {
    $("ask-error").textContent = String(err);
    $("ask-error").hidden = false;
  } finally {
    okBtn.disabled = false;
    $("ask-busy").hidden = true;
    $("ask-input").disabled = false;
    $("ask-cancel").textContent = t("Annuler");
  }
});

// ---- Avatar picker (profile and salons) ----
const pickerDlg = $("picker");
const pk = { value: "", fallbackName: undefined, onOk: null };

function paintPicker() {
  paintAvatar($("pk-preview"), pk.value, pk.fallbackName);
  $("pk-preview").classList.toggle("square", pk.fallbackName !== undefined);
  const p = parseAvatar(pk.value || DEFAULT_AVATAR);
  const emojiMode = !!pk.value && !p.image;
  $("pk-emojis").querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", emojiMode && b.textContent === p.emoji));
  $("pk-colors").querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", emojiMode && b.dataset.color === p.color));
}

function currentEmojiParts() {
  const p = parseAvatar(pk.value && !pk.value.startsWith("img:") ? pk.value : DEFAULT_AVATAR);
  return p;
}

$("pk-emojis").append(
  ...EMOJIS.map((e) => {
    const b = el("button", "", e);
    b.type = "button";
    b.onclick = () => { pk.value = `${e}|${currentEmojiParts().color}`; paintPicker(); };
    return b;
  })
);
$("pk-colors").append(
  ...COLORS.map((c) => {
    const b = el("button");
    b.type = "button";
    b.dataset.color = c;
    b.style.background = c;
    b.title = c;
    b.onclick = () => { pk.value = `${currentEmojiParts().emoji}|${c}`; paintPicker(); };
    return b;
  })
);

function openPicker({ title, value, fallbackName, onOk }) {
  Object.assign(pk, { value, fallbackName, onOk });
  $("picker-title").textContent = title;
  $("pk-reset").hidden = fallbackName === undefined;
  $("picker-error").hidden = true;
  paintPicker();
  pickerDlg.showModal();
}

// Square-crops and shrinks an image file to a small WebP for the cache.
async function shrinkImage(file) {
  const url = URL.createObjectURL(file);
  try {
    const img = await new Promise((ok, ko) => {
      const i = new Image();
      i.onload = () => ok(i);
      i.onerror = () => ko(new Error(t("image illisible")));
      i.src = url;
    });
    const size = 128;
    const canvas = Object.assign(document.createElement("canvas"), { width: size, height: size });
    const side = Math.min(img.width, img.height);
    canvas.getContext("2d").drawImage(img, (img.width - side) / 2, (img.height - side) / 2, side, side, 0, 0, size, size);
    for (const q of [0.85, 0.7, 0.5]) {
      const blob = await new Promise((ok) => canvas.toBlob(ok, "image/webp", q));
      if (blob && blob.size <= 46 * 1024) return new Uint8Array(await blob.arrayBuffer());
    }
    throw new Error(t("image trop complexe, essayez-en une autre"));
  } finally {
    URL.revokeObjectURL(url);
  }
}

$("pk-import").addEventListener("click", () => $("pk-file").click());
$("pk-file").addEventListener("change", async (e) => {
  const file = e.target.files[0];
  e.target.value = "";
  if (!file) return;
  try {
    const bytes = await shrinkImage(file);
    const hash = await invoke("save_image", { bytes: Array.from(bytes) });
    pk.value = "img:" + hash;
    paintPicker();
  } catch (err) {
    $("picker-error").textContent = String(err.message || err);
    $("picker-error").hidden = false;
  }
});
$("pk-reset").addEventListener("click", () => { pk.value = ""; paintPicker(); });
$("picker-cancel").addEventListener("click", () => pickerDlg.close());
$("picker-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const okBtn = $("picker-ok");
  okBtn.disabled = true;
  okBtn.textContent = t("Enregistrement…");
  try {
    await pk.onOk(pk.value);
    pickerDlg.close();
  } catch (err) {
    $("picker-error").textContent = String(err);
    $("picker-error").hidden = false;
  } finally {
    okBtn.disabled = false;
    okBtn.textContent = t("Valider");
  }
});

// ---- Salon actions ----
const current = () => state.salons.find((x) => x.id === selected);

$("main-switch").addEventListener("change", (e) => invoke("set_enabled", { on: e.target.checked }));
$("btn-create").addEventListener("click", () =>
  ask({ title: t("Créer un salon"), input: "", placeholder: t("Nom du salon"), ok: t("Créer"),
        busy: t("Création du salon et publication sur le réseau, cela prend 10 à 30 secondes…"),
        run: async (name) => select(await invoke("create_salon", { name })) })
);
// `code` comes from a clicked invite link: the user still confirms.
function joinSalon(code = "") {
  const known = code && state.salons.find((x) => x.invite === code && !x.deleted);
  if (known) {
    select(known.id);
    toast(t("Vous êtes déjà dans « {name} »", { name: known.name }));
    return;
  }
  // Replaces any open dialog (an operation in progress goes on in the background).
  if (askDlg.open) askDlg.close();
  ask({ title: t("Rejoindre un salon"),
        text: t(code ? "Vous avez ouvert un lien d'invitation. Rejoindre ce salon ?" : "Saisissez le code ou le lien d'invitation reçu."),
        input: code, placeholder: t("ex. K7QM-ZXRA-PQ4H"), ok: t("Rejoindre"),
        busy: t("Recherche de l'invitation sur le réseau, cela prend 10 à 30 secondes…"),
        run: async (code) => select(await invoke("join_invite", { code })) });
}
$("btn-join").addEventListener("click", () => joinSalon());

const inviteLink = (code) => `uberconnector://join/${code}`;
// Static redirect page (web/index.html) that makes the link clickable in
// Discord & co, which only linkify http(s). Hosted on Cloudflare (free).
const INVITE_PAGE = "https://uc.nynjardin.workers.dev/";
$("btn-copy-invite").addEventListener("click", (e) => {
  const cur = current();
  if (cur.deleted) return;
  const items = [];
  if (INVITE_PAGE) items.push({ label: t("Copier le lien web (cliquable partout)"), run: () => copy(`${INVITE_PAGE}#${cur.invite}`) });
  items.push(
    { label: t("Copier le lien uberconnector://"), run: () => copy(inviteLink(cur.invite)) },
    { label: t("Copier le code ({code})", { code: cur.invite }), run: () => copy(cur.invite) },
  );
  showMenu(e, items);
});

// Invite link clicked while the app runs, or that launched it.
async function takeInvite() {
  if (!state) return; // taken once the first state is rendered
  const code = await invoke("take_invite");
  if (code) joinSalon(code);
}
listen("invite", takeInvite);
$("btn-game").addEventListener("click", () => {
  const cur = current();
  invoke("set_game", { id: state.settings.game_salon === cur.id ? null : cur.id });
});
$("btn-connect").addEventListener("click", () => {
  const cur = current();
  if (state.settings.connected.includes(cur.id)) invoke("disconnect", { id: cur.id });
  else invoke("connect_salon", { id: cur.id });
});
$("cur-avatar").addEventListener("click", () => {
  const cur = current();
  if (!cur.is_owner || cur.deleted) return;
  openPicker({
    title: t("Avatar de « {name} »", { name: cur.name }),
    value: cur.avatar,
    fallbackName: cur.name,
    onOk: (avatar) => invoke("set_salon_avatar", { id: cur.id, avatar }),
  });
});
function renameSalon() {
  const cur = current();
  ask({ title: t("Renommer le salon"), input: cur.name, ok: t("Renommer"), busy: t("Publication du nouveau nom…"), run: (name) => invoke("rename_salon", { id: cur.id, name }) });
}
function deleteSalon() {
  const cur = current();
  ask({
    title: t("Supprimer « {name} » ?", { name: cur.name }),
    text: t("Le salon disparaîtra pour tous ses membres et le code d'invitation ne fonctionnera plus. C'est définitif."),
    ok: t("Supprimer"),
    danger: true,
    busy: t("Suppression du salon sur le réseau…"),
    run: () => invoke("delete_salon", { id: cur.id }),
  });
}
function forgetSalon() {
  const cur = current();
  ask({
    title: t("Retirer « {name} » ?", { name: cur.name }),
    text: t(cur.deleted ? "Ce salon a été supprimé par son propriétaire." : "Vous pourrez y revenir plus tard avec son code d'invitation."),
    ok: t("Retirer"),
    run: () => invoke("forget_salon", { id: cur.id }),
  });
}
document.querySelectorAll("[data-copy]").forEach((b) =>
  b.addEventListener("click", () => {
    const text = $(b.dataset.copy).textContent;
    if (text && text !== "–" && text !== "—") copy(text);
  })
);

// Collapsible salons pane (avatars only).
function setCollapsed(on) {
  $("layout").classList.toggle("collapsed", on);
  $("btn-collapse").title = t(on ? "Agrandir le volet" : "Réduire le volet");
  store.set("collapsed", on ? "1" : "");
}
$("btn-collapse").addEventListener("click", () => setCollapsed(!$("layout").classList.contains("collapsed")));

// ---- Profile ----
const profileDlg = $("profile");
let pfAvatar = "";
$("btn-profile").addEventListener("click", () => {
  const s = state.settings;
  pfAvatar = s.avatar;
  paintAvatar($("pf-preview"), pfAvatar);
  $("pf-name").value = s.name;
  $("pf-voice-auto").checked = s.voice_auto_join;
  $("pf-voice-muted").checked = s.voice_start_muted;
  $("profile-error").hidden = true;
  profileDlg.showModal();
});
$("pf-avatar").addEventListener("click", () =>
  openPicker({
    title: t("Mon avatar"),
    value: pfAvatar,
    onOk: async (a) => { pfAvatar = a || DEFAULT_AVATAR; paintAvatar($("pf-preview"), pfAvatar); },
  })
);
$("profile-cancel").addEventListener("click", () => profileDlg.close());
$("profile-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const settings = {
    ...state.settings,
    name: $("pf-name").value.trim(),
    avatar: pfAvatar,
    voice_auto_join: $("pf-voice-auto").checked,
    voice_start_muted: $("pf-voice-muted").checked,
  };
  try {
    await invoke("save_settings", { settings });
    profileDlg.close();
    toast(t("Profil enregistré"));
  } catch (err) {
    $("profile-error").textContent = String(err);
    $("profile-error").hidden = false;
  }
});

// ---- Settings ----
const dlg = $("settings");
$("btn-settings").addEventListener("click", () => {
  const s = state.settings;
  $("set-autostart").checked = s.autostart;
  $("set-update-check").checked = s.update_check;
  $("update-result").textContent = "";
  $("set-port-map").checked = s.port_map;
  $("set-custom-port").value = s.custom_port || "";
  $("set-notify-online").checked = s.notify_online;
  $("set-notify-knock").checked = s.notify_knock;
  $("set-notify-mention").checked = s.notify_mention;
  $("set-debug").checked = s.debug;
  $("set-lang").value = s.lang;
  $("set-relays").value = s.relays.join("\n");
  $("settings-error").hidden = true;
  dlg.showModal();
});
$("settings-cancel").addEventListener("click", () => dlg.close());
$("settings-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const settings = {
    ...state.settings,
    autostart: $("set-autostart").checked,
    update_check: $("set-update-check").checked,
    port_map: $("set-port-map").checked,
    custom_port: parseInt($("set-custom-port").value, 10) || 0,
    notify_online: $("set-notify-online").checked,
    notify_knock: $("set-notify-knock").checked,
    notify_mention: $("set-notify-mention").checked,
    debug: $("set-debug").checked,
    lang: $("set-lang").value,
    relays: $("set-relays").value.split("\n").map((r) => r.trim()).filter(Boolean),
  };
  try {
    await invoke("save_settings", { settings });
    dlg.close();
    // Static texts are translated once: reload the page in the new language.
    if ((await invoke("ui_lang")) !== LANG) return location.reload();
    toast(t("Réglages enregistrés"));
  } catch (err) {
    $("settings-error").textContent = String(err);
    $("settings-error").hidden = false;
  }
});

// ---- Technical log (development) ----
function renderLog() {
  const cur = state && current();
  $("log-salon").textContent = cur ? `· ${cur.name}` : "";
  const st = cur && state.statuses[cur.id];
  const events = st?.events || [];
  const timings = st?.timings || [];
  const lines = !st ? [t("(rien : salon non connecté)")]
    : [...(timings.length ? [t("⏱ Chronologie de la connexion"), ...timings, ""] : []), ...events];
  $("events").replaceChildren(...lines.map((e) => el("li", "", e)));
}
$("btn-log").addEventListener("click", () => { renderLog(); $("log").showModal(); });
$("log-close").addEventListener("click", () => $("log").close());
$("log-copy").addEventListener("click", async () => {
  const cur = current();
  if (!cur) return;
  await copy(await invoke("diagnostic", { id: cur.id }));
  toast(t("Diagnostic copié : collez-le dans un message"));
});

// ---- Updates (GitHub releases) ----
let update = null;
function showUpdate(u) {
  update = u;
  $("update-text").textContent = t("UberConnector {version} est disponible (vous avez la {current}).", { version: u.version, current: appVersion });
  $("update-go").disabled = false;
  $("update-notes").hidden = !u.notes.trim();
  $("update-bar").hidden = false;
}
function installUpdate() {
  $("update-go").disabled = true;
  $("update-text").textContent = t("Mise à jour vers {version} : arrêt du moteur…", { version: update.version });
  invoke("install_update").catch((err) => {
    $("update-text").textContent = t("Mise à jour impossible : {err}", { err });
    $("update-go").disabled = false;
  });
}
let appVersion = "";
invoke("app_version").then((v) => {
  appVersion = v;
  $("app-version").textContent = v;
});
listen("update", ({ payload }) => showUpdate(payload));
listen("update-progress", ({ payload: [done, total] }) => {
  const pct = total ? ` ${Math.floor((done * 100) / total)} %` : "";
  $("update-text").textContent = t("Téléchargement de la version {version}…{pct}", { version: update.version, pct });
});
$("update-go").addEventListener("click", installUpdate);
$("update-later").addEventListener("click", () => ($("update-bar").hidden = true));
$("update-notes").addEventListener("click", () =>
  ask({ title: t("Nouveautés de la version {version}", { version: update.version }), text: update.notes, ok: t("Mettre à jour"), run: async () => installUpdate() })
);
$("btn-check-update").addEventListener("click", async () => {
  const b = $("btn-check-update");
  b.disabled = true;
  $("update-result").textContent = t("· recherche…");
  try {
    const u = await invoke("check_update");
    if (u) {
      $("update-result").textContent = t("· version {version} disponible", { version: u.version });
      showUpdate(u);
    } else {
      $("update-result").textContent = t("· vous avez la dernière version");
    }
  } catch (err) {
    $("update-result").textContent = t("· impossible de vérifier : {err}", { err });
  } finally {
    b.disabled = false;
  }
});

// Rendered once the language is known and the first state is shown.
listen("state", (e) => state && render(e.payload));
listen("notice", ({ payload }) => toast(payload));
// Click on a Windows notification: show its salon.
listen("select", ({ payload: id }) => select(id));
invoke("ui_lang").then(async (lang) => {
  setLang(lang);
  translatePage(document.body);
  setCollapsed(store.get("collapsed") === "1");
  const s = await invoke("get_state");
  // Lines received before this page loaded (e.g. window reopened from the tray).
  for (const id of s.settings.connected) chats[id] = await invoke("get_chat", { id });
  render(s);
  takeInvite();
});
