// Éditeur de montage (étape ③) : frise avec les clips en blocs vidéo (dans l'ordre, à leur place
// réelle avec les transitions) et des pistes audio libres : musiques posées où l'on veut, rognées,
// en fondu, avec leur volume. Écoute du mixage avec une tête de lecture : pistes audio et son
// d'origine des clips (proxys .lrv ; les passages accélérés restent muets, comme à l'export).
//
// Les pistes sont dans le style du projet (`audio_tracks`, validé par core/src/finishing.rs) :
//   {id, file, start, offset, length, volume, fade_in, fade_out, loop, muted}
//   start : instant du montage (s) · offset : début dans le fichier · length : durée jouée
//   (null = jusqu'à la fin du fichier ou du montage) · loop : le fichier se répète.
// La durée des blocs vidéo et les chevauchements de transition reprennent finish_command.
//
// L'aperçu vidéo (visionneuse) suit la tête de lecture : le clip sous la tête s'y affiche et joue,
// avec son son d'origine ; hook show(clip, instant, lecture) fourni par montage.js.
// Partage : initEditor, renderEditor, addTrack, setEditorStatus, togglePreview.

import { st, video } from "./state.js";
import { $, api, apiOrError, esc, fmt, storageGet, storageSet } from "./util.js";
import { outputDuration, speedAt, speedKeys } from "./speed.js";
import { onStep } from "./steps.js";

const END_CARD_S = 5;                 // = finishing::END_CARD_S
let HEAD_W = 116;                     // colonne des intitulés (px), lue dans --mte-head
const SNAP_PX = 9;                    // aimantation (px)
const MIN_LEN = 0.2;                  // durée minimale d'une piste (s)
const ORIG_MUTED_VOLUME = 0;
const ZOOMS = [1, 1.5, 2, 3, 4, 6, 8, 12, 16, 24];

let hooks = { reorder() {}, exclude() {}, open() {}, show() {} };
let root = null;
let files = new Map();                // nom → durée (s)
let zoom = 0;                         // indice dans ZOOMS
let selected = null;                  // {kind: "clip"|"track"|"orig", id}
let playhead = 0;
let drag = null;
let saveTimer = null;
let origBeforeMute = 0.35;
const peaks = new Map();              // fichier → {per_s, peaks} | "loading" | "none"

const style = () => st.project.style || (st.project.style = {});
const tracksOf = () => style().audio_tracks || [];
const keyOf = (c) => `${c.sid}|${c.id}`;
const baseName = (f) => f.replace(/\.[^.]+$/, "");

// ------------------------------------------------------------------ disposition de la frise

/** Place de chaque bloc vidéo sur la frise : mêmes règles que finish_command (chevauchement des transitions). */
function layout() {
  const s = style();
  const clips = st.project.clips.filter((c) => !c.excluded);
  const lens = clips.map(outputDuration);
  const card = !!s.end_card && clips.length > 0;
  const all = card ? [...lens, END_CARD_S] : lens;
  const kind = (s.transition && s.transition !== "aucune") || card;
  const t = kind && all.length > 1 ? Math.min(+s.duration || 0.6, Math.min(...all) / 2) : 0;
  let acc = 0;
  const starts = all.map((d, i) => { const at = acc; acc += d - (i < all.length - 1 ? t : 0); return at; });
  const blocks = clips.map((c, i) => ({ c, start: starts[i], len: lens[i] }));
  const total = all.length ? starts[all.length - 1] + all[all.length - 1] : 0;
  return { blocks, card: card ? { start: starts[all.length - 1], len: END_CARD_S } : null, total, t };
}

/** Durée jouée d'une piste (s) : bornée par le fichier (sauf répétition) et par la fin du montage. */
function playedLength(tr, total) {
  const avail = tr.loop ? Infinity : Math.max(0, (files.get(tr.file) ?? 0) - tr.offset);
  const len = Math.min(tr.length ?? Infinity, avail, Math.max(0, total - tr.start));
  return Number.isFinite(len) ? len : 0;
}

let geo = { blocks: [], card: null, total: 0, t: 0 };
let pps = 10;                         // pixels par seconde

function computePps() {
  const scroll = root.querySelector(".mte-scroll");
  const width = Math.max(200, (scroll?.clientWidth || 600) - HEAD_W - 16);
  const fit = width / Math.max(geo.total, 10);
  pps = fit * ZOOMS[zoom];
}
const px = (s) => Math.round(s * pps * 10) / 10;

// ------------------------------------------------------------------ enregistrement

function save(extra = {}) {
  const patch = { audio_tracks: tracksOf(), original_volume: style().original_volume, ...extra };
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => api("PUT", "/api/project", { style: patch }).catch(() => setEditorStatus("⚠ enregistrement impossible")), 350);
}
function setTracks(list) { st.project.style = { ...style(), audio_tracks: list }; }
function patchTrack(id, p) {
  setTracks(tracksOf().map((t) => (t.id === id ? { ...t, ...p } : t)));
  save();
}

export function setEditorStatus(text) { const el = root?.querySelector(".mte-status"); if (el) el.textContent = text; }

// ------------------------------------------------------------------ pistes : ajout, retrait

/** Ajoute le fichier audio `name` (déjà présent côté serveur) comme nouvelle piste. */
export async function addTrack(name) {
  await loadFiles();
  const list = tracksOf();
  if (list.length >= 8) { setEditorStatus("⚠ 8 pistes au plus"); return; }
  const id = "a" + (Math.max(0, ...list.map((t) => +t.id.slice(1) || 0)) + 1);
  const first = !list.length;
  // première musique : le son d'origine passe au second plan (réglable ensuite)
  if (first && (style().original_volume ?? 1) >= 0.99) style().original_volume = 0.35;
  const t = { id, file: name, start: first ? 0 : Math.round(Math.min(playhead, Math.max(0, geo.total - 1)) * 10) / 10,
              offset: 0, length: null, volume: 0.8, fade_in: first ? 1.5 : 0.5, fade_out: first ? 3 : 1, loop: false, muted: false };
  setTracks([...list, t]);
  selected = { kind: "track", id };
  save();
  renderEditor();
  setEditorStatus(`✓ ${baseName(name)} ajoutée`);
}

function removeTrack(id) {
  setTracks(tracksOf().filter((t) => t.id !== id));
  stopPreview(id);
  selected = null;
  save();
  renderEditor();
}

async function loadFiles() {
  try {
    const list = await api("GET", "/api/music/files");
    files = new Map(list.map((f) => [f.name, f.seconds]));
  } catch (e) { /* serveur injoignable : on garde la liste connue */ }
}

function getPeaks(file) {
  const p = peaks.get(file);
  if (p && typeof p === "object") return p;
  if (!p) {
    peaks.set(file, "loading");
    fetch(`/api/music/peaks?name=${encodeURIComponent(file)}`).then((r) => r.json())
      .then((j) => { peaks.set(file, j.peaks ? j : "none"); if (!drag) renderEditor(); })
      .catch(() => peaks.set(file, "none"));
  }
  return null;
}

// ------------------------------------------------------------------ rendu

function mount() {
  root.innerHTML = `
    <div class="mte-bar">
      <button data-a="play" class="primary" title="Écouter le mixage (son d'origine et musiques) depuis la tête de lecture">▶</button>
      <span class="mte-time">0:00 / 0:00</span>
      <span class="spacer"></span>
      <label class="toggle" title="Aimante les pistes aux bords des clips et à la tête de lecture"><input type="checkbox" class="mte-snap"> aimanter</label>
      <button data-a="zoom-out" title="Zoom arrière" aria-label="Zoom arrière">−</button>
      <button data-a="zoom-fit" title="Tout voir" aria-label="Tout voir">⤢</button>
      <button data-a="zoom-in" title="Zoom avant" aria-label="Zoom avant">＋</button>
    </div>
    <div class="mte-scroll"><div class="mte-rows"></div></div>
    <div class="mte-add row">
      <button id="lib-open" title="Musiques libres (CC BY) de Kevin MacLeod, créditées à la fin du montage">🎵 Musiques libres…</button>
      <label class="upload" title="Envoyer un fichier audio (mp3, m4a, wav…)">＋ Fichier audio<input type="file" class="mte-upload" accept="audio/*" hidden></label>
      <select class="mte-pick" title="Ajouter un fichier déjà envoyé"><option value="">＋ Déjà envoyés…</option></select>
    </div>
    <div class="mte-insp"></div>
    <span class="hint mte-status"></span>`;
  const snap = root.querySelector(".mte-snap");
  snap.checked = storageGet("bike360.mte.snap") !== "0";
  snap.addEventListener("change", () => storageSet("bike360.mte.snap", snap.checked ? "1" : "0"));
  root.addEventListener("click", onClick);
  root.addEventListener("input", onInput);
  root.addEventListener("change", onChange);
  root.addEventListener("pointerdown", onPointerDown);
  root.addEventListener("wheel", (e) => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    setZoom(zoom + (e.deltaY < 0 ? 1 : -1));
  }, { passive: false });
  new ResizeObserver(() => { if (!drag && root.offsetParent) renderEditor(); }).observe(root.querySelector(".mte-scroll"));
}

export function renderEditor() {
  if (!root || drag) return;
  HEAD_W = parseInt(getComputedStyle(root).getPropertyValue("--mte-head")) || 116;
  geo = layout();
  computePps();
  const w = px(geo.total) + 24;
  const s = style(), tracks = tracksOf();
  const rows = [];

  // règle
  let step = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800].find((x) => x * pps >= 64) || 3600;
  const ticks = [];
  for (let t = 0; t <= geo.total + 0.01; t += step) ticks.push(`<span style="left:${px(t)}px">${fmt(t)}</span>`);
  rows.push(`<div class="mte-row ruler"><div class="mte-head"></div><div class="mte-lane" data-lane="ruler" style="width:${w}px">${ticks.join("")}</div></div>`);

  // clips
  const blocks = geo.blocks.map((b, i) => {
    const c = b.c, t = (c.start + Math.min(2, (c.end - c.start) / 2)).toFixed(1);
    return `<div class="blk blk-clip${selected?.kind === "clip" && selected.id === keyOf(c) ? " sel" : ""}" data-key="${esc(keyOf(c))}"
      style="left:${px(b.start)}px;width:${Math.max(6, px(b.len))}px;background-image:url('/thumb/${c.sid}.jpg?t=${t}&yaw=${c.yaw}&pitch=${c.pitch}&fov=${c.fov}')"
      title="Clip ${i + 1} · ${fmt(b.len)} — glisser pour le déplacer"><span class="n">${i + 1}</span><span class="d">${fmt(b.len)}</span></div>`;
  }).join("");
  const card = geo.card ? `<div class="blk blk-card" style="left:${px(geo.card.start)}px;width:${px(geo.card.len)}px" title="Carte de fin et statistiques">fin</div>` : "";
  const over = geo.t > 0 ? geo.blocks.slice(1).concat(geo.card ? [{ start: geo.card.start }] : []).map((b) =>
    `<i class="xf" style="left:${px(b.start)}px;width:${px(geo.t)}px"></i>`).join("") : "";
  rows.push(`<div class="mte-row"><div class="mte-head"><b>Vidéo</b><small>${geo.blocks.length} clip(s)</small></div>
    <div class="mte-lane lane-video" data-lane="video" style="width:${w}px">${blocks}${card}${over}<i class="drop" hidden></i>
    ${geo.blocks.length ? "" : `<span class="empty">Aucun clip : repère des moments à l'étape ②, ou ✨ montage auto.</span>`}</div></div>`);

  // son d'origine
  const ov = s.original_volume ?? 1;
  rows.push(`<div class="mte-row${selected?.kind === "orig" ? " sel" : ""}" data-orig><div class="mte-head">
      <b>Son d'origine</b>
      <span class="ctl"><button data-a="orig-mute" title="${ov > 0 ? "Couper" : "Rétablir"} le son d'origine" aria-label="Son d'origine">${ov > 0 ? "🔊" : "🔇"}</button>
      <input type="range" min="0" max="1.5" step="0.05" value="${ov}" data-orig-vol aria-label="Volume du son d'origine"></span></div>
    <div class="mte-lane lane-orig" data-lane="orig" style="width:${w}px"><div class="orig-bar" style="width:${px(geo.total - (geo.card ? END_CARD_S : 0))}px;opacity:${Math.min(1, 0.25 + ov * 0.6)}"></div></div></div>`);

  // pistes audio
  for (const tr of tracks) {
    const len = playedLength(tr, geo.total), known = files.has(tr.file);
    const dim = tr.muted || tr.volume <= 0;
    const loopMarks = tr.loop && known ? Array.from({ length: Math.floor(len / files.get(tr.file)) }, (_, k) =>
      `<i class="lp" style="left:${px((k + 1) * files.get(tr.file))}px"></i>`).join("") : "";
    rows.push(`<div class="mte-row${selected?.kind === "track" && selected.id === tr.id ? " sel" : ""}" data-tid="${esc(tr.id)}">
      <div class="mte-head"><b title="${esc(tr.file)}">${esc(baseName(tr.file))}</b>
        <span class="ctl"><button data-a="mute" title="${dim ? "Rétablir" : "Couper"} la piste" aria-label="Couper la piste">${dim ? "🔇" : "🔊"}</button>
        <input type="range" min="0" max="1.5" step="0.05" value="${tr.volume}" data-vol aria-label="Volume de la piste"></span></div>
      <div class="mte-lane" data-lane="track" style="width:${w}px">
        <div class="blk blk-aud${dim ? " dim" : ""}${known ? "" : " missing"}" data-tid="${esc(tr.id)}"
          style="left:${px(tr.start)}px;width:${Math.max(14, px(known ? len : 3))}px">
          <canvas></canvas>${loopMarks}
          <span class="hdl hdl-l" data-h="l"></span><span class="hdl hdl-r" data-h="r"></span>
          <span class="lbl">${known ? `${esc(baseName(tr.file))} · ${fmt(len)}` : "fichier manquant"}</span></div></div></div>`);
  }
  rows.push(`<div class="mte-cursor" style="left:${HEAD_W + px(playhead)}px"></div>`);

  const sc = root.querySelector(".mte-scroll"), left = sc.scrollLeft;
  const rowsEl = root.querySelector(".mte-rows");
  rowsEl.style.width = `${HEAD_W + w}px`;
  rowsEl.innerHTML = rows.join("");
  sc.scrollLeft = left;
  drawWaves();
  renderInspector();
  renderTime();

  const pick = root.querySelector(".mte-pick");
  pick.innerHTML = `<option value="">＋ Déjà envoyés…</option>` + [...files.keys()].map((f) => `<option value="${esc(f)}">${esc(baseName(f))} (${fmt(files.get(f))})</option>`).join("");
  root.querySelector('[data-a="play"]').textContent = player ? "❚❚" : "▶";
  root.querySelector('[data-a="zoom-out"]').disabled = zoom === 0;
  root.querySelector('[data-a="zoom-in"]').disabled = zoom === ZOOMS.length - 1;
}

/** Forme d'onde de chaque piste (amplitude × volume), avec l'enveloppe des fondus. */
function drawWaves() {
  for (const tr of tracksOf()) {
    const blk = root.querySelector(`.blk-aud[data-tid="${CSS.escape(tr.id)}"]`);
    const cv = blk?.querySelector("canvas");
    if (!cv || !files.has(tr.file)) continue;
    const len = playedLength(tr, geo.total), W = Math.min(2400, Math.max(1, Math.round(px(len)))), H = 44;
    cv.width = W; cv.height = H;
    const ctx = cv.getContext("2d"), pk = getPeaks(tr.file), dur = files.get(tr.file);
    const env = (t) => Math.min(1, tr.fade_in > 0 ? t / tr.fade_in : 1, tr.fade_out > 0 ? (len - t) / tr.fade_out : 1);
    ctx.fillStyle = "rgba(255,255,255,.55)";
    if (pk) {
      for (let x = 0; x < W; x++) {
        const t = (x / W) * len;
        let ft = tr.offset + t;
        if (tr.loop) ft %= dur;
        const a = pk.peaks[Math.min(pk.peaks.length - 1, Math.floor(ft * pk.per_s))] || 0;
        const h = Math.max(1, a * Math.min(1, 0.35 + tr.volume * 0.65) * H * 0.9);
        ctx.fillRect(x, (H - h) / 2, 1, h);
      }
    }
    ctx.strokeStyle = "rgba(0,0,0,.65)"; ctx.lineWidth = 2; ctx.beginPath();   // enveloppe des fondus
    for (let x = 0; x <= W; x += 4) {
      const y = H - 2 - env((x / W) * len) * (H - 4);
      x ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
    }
    ctx.stroke();
  }
}

function renderTime() {
  const el = root.querySelector(".mte-time");
  if (el) el.textContent = `${fmt(playhead)} / ${fmt(geo.total)}`;
  const cur = root.querySelector(".mte-cursor");
  if (cur) cur.style.left = `${HEAD_W + px(playhead)}px`;
}

// ------------------------------------------------------------------ inspecteur

function renderInspector() {
  const el = root.querySelector(".mte-insp");
  const slider = (key, label, min, max, step, v, unit) =>
    `<label class="field slider"><span>${label} <b data-out="${key}">${unit(v)}</b></span><input type="range" data-f="${key}" min="${min}" max="${max}" step="${step}" value="${v}"></label>`;
  const pct = (v) => Math.round(v * 100) + " %", sec = (v) => (+v).toFixed(1).replace(".", ",") + " s";
  if (selected?.kind === "track") {
    const tr = tracksOf().find((t) => t.id === selected.id);
    if (!tr) { selected = null; return renderInspector(); }
    const len = playedLength(tr, geo.total);
    el.innerHTML = `<div class="insp-title"><b>${esc(baseName(tr.file))}</b>
        <span class="muted">débute à ${fmt(tr.start)} · joue ${fmt(len)}${tr.offset ? ` · à partir de ${fmt(tr.offset)} dans le fichier` : ""}</span></div>
      <div class="insp-grid">
        ${slider("volume", "Volume", 0, 1.5, 0.05, tr.volume, pct)}
        ${slider("fade_in", "Fondu d'entrée", 0, 10, 0.5, tr.fade_in, sec)}
        ${slider("fade_out", "Fondu de sortie", 0, 10, 0.5, tr.fade_out, sec)}</div>
      <div class="row">
        <label class="check" title="Le fichier se répète jusqu'à la fin de la piste"><input type="checkbox" data-f="loop" ${tr.loop ? "checked" : ""}> Répéter</label>
        <button data-a="t-start" title="Place le début de la piste à la tête de lecture">⇤ Début ici</button>
        <button data-a="t-end" title="Termine la piste à la tête de lecture">⇥ Fin ici</button>
        <button data-a="t-fill" title="Joue jusqu'à la fin du fichier ou du montage">Jusqu'à la fin</button>
        <button data-a="t-dup" title="Copie la piste">⧉ Dupliquer</button>
        <button data-a="t-del" class="danger" title="Retirer la piste du montage">🗑 Retirer</button></div>`;
  } else if (selected?.kind === "clip") {
    const i = geo.blocks.findIndex((b) => keyOf(b.c) === selected.id), b = geo.blocks[i];
    if (!b) { selected = null; return renderInspector(); }
    el.innerHTML = `<div class="insp-title"><b>Clip ${i + 1}</b><span class="muted">${fmt(b.len)}${b.len !== b.c.end - b.c.start ? " (accéléré)" : ""} · débute à ${fmt(b.start)}</span></div>
      <div class="row">
        <button data-a="c-left" ${i ? "" : "disabled"}>← Avancer</button><button data-a="c-right" ${i < geo.blocks.length - 1 ? "" : "disabled"}>Reculer →</button>
        <button data-a="c-open" title="Revoir le clip dans sa session">👁 Voir le clip</button>
        <button data-a="c-ex" title="Retirer du montage (reste dans la liste)">◌ Exclure</button></div>`;
  } else if (selected?.kind === "orig") {
    const v = style().original_volume ?? 1;
    el.innerHTML = `<div class="insp-title"><b>Son d'origine</b><span class="muted">moteur, vent, ambiance de la caméra</span></div>
      <div class="insp-grid">${slider("original_volume", "Volume", 0, 1.5, 0.05, v, pct)}</div>`;
  } else {
    el.innerHTML = `<span class="hint">Touche un clip ou une piste pour la régler. Glisse un clip pour changer l'ordre, une piste pour la placer ; ses bords la rognent.</span>`;
  }
}

// ------------------------------------------------------------------ commandes

function setZoom(z) {
  const sc = root.querySelector(".mte-scroll");
  const centre = (sc.scrollLeft + (sc.clientWidth + HEAD_W) / 2 - HEAD_W) / pps;   // instant au milieu de la partie visible
  zoom = Math.max(0, Math.min(ZOOMS.length - 1, z));
  renderEditor();
  sc.scrollLeft = zoom === 0 ? 0 : Math.max(0, HEAD_W + centre * pps - (sc.clientWidth + HEAD_W) / 2);
}

function onClick(e) {
  const b = e.target.closest("[data-a]");
  if (!b) {
    const row = e.target.closest(".mte-row[data-tid], .mte-row[data-orig]");
    if (!row) return;
    if (e.target.closest(".mte-lane") && !e.target.closest(".blk")) seekTo(timeAt(e.clientX));   // toucher une piste vide : y placer la tête de lecture
    if (!e.target.closest("input, .blk")) select(row.dataset.tid ? { kind: "track", id: row.dataset.tid } : { kind: "orig" });
    return;
  }
  const a = b.dataset.a, tid = selected?.kind === "track" ? selected.id : null;
  const tr = tid && tracksOf().find((t) => t.id === tid);
  const rowTid = b.closest("[data-tid]")?.dataset.tid;
  switch (a) {
    case "play": return player ? stopPreview() : startPreview();
    case "zoom-in": return setZoom(zoom + 1);
    case "zoom-out": return setZoom(zoom - 1);
    case "zoom-fit": return setZoom(0);
    case "mute": {
      const t = tracksOf().find((x) => x.id === rowTid);
      return t && patchTrackThenRender(t.id, { muted: !(t.muted || t.volume <= 0), volume: t.volume <= 0 ? 0.8 : t.volume });
    }
    case "orig-mute": {
      const v = style().original_volume ?? 1;
      if (v > 0) origBeforeMute = v;
      style().original_volume = v > 0 ? ORIG_MUTED_VOLUME : origBeforeMute;
      save(); return renderEditor();
    }
    case "t-start": return tr && patchTrackThenRender(tid, trimStartTo(tr, playhead));
    case "t-end": return tr && patchTrackThenRender(tid, { length: Math.max(MIN_LEN, Math.round((playhead - tr.start) * 10) / 10) });
    case "t-fill": return tr && patchTrackThenRender(tid, { length: null });
    case "t-del": return tr && removeTrack(tid);
    case "t-dup": {
      if (!tr || tracksOf().length >= 8) return;
      const id = "a" + (Math.max(0, ...tracksOf().map((t) => +t.id.slice(1) || 0)) + 1);
      setTracks([...tracksOf(), { ...tr, id, start: Math.round(Math.min(geo.total, tr.start + playedLength(tr, geo.total)) * 10) / 10 }]);
      selected = { kind: "track", id }; save(); return renderEditor();
    }
    case "c-left": case "c-right": return moveClip(selected.id, a === "c-left" ? -1 : 1);
    case "c-ex": { const c = geo.blocks.find((x) => keyOf(x.c) === selected?.id)?.c; selected = null; return c && hooks.exclude(c); }
    case "c-open": { const c = geo.blocks.find((x) => keyOf(x.c) === selected?.id)?.c; return c && hooks.open(c); }
  }
}

/** Début de piste à `t` : la fin et le contenu restent en place (le début dans le fichier suit). */
function trimStartTo(tr, t) {
  const r = (x) => Math.round(x * 100) / 100;
  let d = Math.round(t * 10) / 10 - tr.start;
  d = Math.max(tr.loop ? -tr.start : Math.max(-tr.start, -tr.offset), Math.min(playedLength(tr, geo.total) - MIN_LEN, d));
  return { start: r(tr.start + d), offset: tr.loop ? 0 : r(tr.offset + d), length: tr.length === null ? null : Math.max(MIN_LEN, r(tr.length - d)) };
}

function patchTrackThenRender(id, p) { patchTrack(id, p); renderEditor(); }

function select(sel) { selected = sel; renderEditor(); }

function moveClip(key, dir) {
  const kept = geo.blocks.map((b) => b.c), i = kept.findIndex((c) => keyOf(c) === key), j = i + dir;
  if (i < 0 || j < 0 || j >= kept.length) return;
  const order = [...kept];
  [order[i], order[j]] = [order[j], order[i]];
  reorderKept(order);
}

/** Nouvel ordre des clips gardés ; les exclus restent à leur place relative. */
function reorderKept(order) {
  const pending = [...order];
  const full = st.project.clips.map((c) => (c.excluded ? c : pending.shift()));
  hooks.reorder(full);
}

function onInput(e) {
  const el = e.target;
  if (el.matches("[data-vol]")) {   // curseur de volume dans l'intitulé de la piste
    const id = el.closest("[data-tid]").dataset.tid, v = +el.value;
    setTracks(tracksOf().map((t) => (t.id === id ? { ...t, volume: v, muted: false } : t)));
    save();
    drawWaves();
  } else if (el.matches("[data-orig-vol]")) {
    style().original_volume = +el.value;
    save();
  } else if (el.matches(".mte-insp [data-f]")) {
    const key = el.dataset.f;
    if (key === "loop") return;
    const v = +el.value;
    if (key === "original_volume") style().original_volume = v;
    else if (selected?.kind === "track") setTracks(tracksOf().map((t) => (t.id === selected.id ? { ...t, [key]: v } : t)));
    const out = root.querySelector(`[data-out="${key}"]`);
    if (out) out.textContent = key.startsWith("fade") ? v.toFixed(1).replace(".", ",") + " s" : Math.round(v * 100) + " %";
    save();
    drawWaves();
  }
}

function onChange(e) {
  const el = e.target;
  if (el.matches(".mte-insp [data-f=loop]") && selected?.kind === "track") {
    patchTrackThenRender(selected.id, { loop: el.checked, offset: el.checked ? 0 : tracksOf().find((t) => t.id === selected.id).offset });
  } else if (el.matches(".mte-upload")) {
    const f = el.files[0];
    if (!f) return;
    setEditorStatus(`Envoi de ${f.name}…`);
    fetch(`/api/music?name=${encodeURIComponent(f.name)}`, { method: "POST", body: f }).then((x) => x.json()).then(async (r) => {
      if (r.error) return setEditorStatus("⚠ " + r.error);
      await addTrack(r.name);
    }).catch(() => setEditorStatus("⚠ envoi impossible")).finally(() => { el.value = ""; });
  } else if (el.matches(".mte-pick") && el.value) {
    const name = el.value;
    el.value = "";
    addTrack(name);
  } else if (el.matches("[data-orig-vol], [data-vol]")) {
    renderEditor();
  }
}

// ------------------------------------------------------------------ gestes (déplacer, rogner, réordonner, tête de lecture)

const snapOn = () => root.querySelector(".mte-snap").checked;
function snapPoints(excludeTid) {
  const pts = [0, geo.total, playhead];
  for (const b of geo.blocks) pts.push(b.start, b.start + b.len);
  for (const t of tracksOf()) if (t.id !== excludeTid) pts.push(t.start, t.start + playedLength(t, geo.total));
  return pts;
}
/** Aimante `v` (ou v+len pour un bloc entier) au point le plus proche si l'écart est petit. */
function snap(v, len, pts) {
  if (!snapOn()) return v;
  let best = null;
  for (const p of pts) for (const [x, off] of [[v, 0], [v + len, len]]) {
    const d = Math.abs(x - p) * pps;
    if (d < SNAP_PX && (!best || d < best.d)) best = { d, v: p - off };
  }
  return best ? best.v : v;
}

const timeAt = (clientX) => {
  const sc = root.querySelector(".mte-scroll");
  return Math.max(0, Math.min(geo.total, (clientX - sc.getBoundingClientRect().left + sc.scrollLeft - HEAD_W) / pps));
};

function onPointerDown(e) {
  if (e.button > 0 || e.target.closest("input, button, select, label")) return;
  const hdl = e.target.closest(".hdl"), aud = e.target.closest(".blk-aud"), clip = e.target.closest(".blk-clip");
  const lane = e.target.closest(".mte-lane");
  if (aud) {
    const tr = tracksOf().find((t) => t.id === aud.dataset.tid);
    if (!tr) return;
    selected = { kind: "track", id: tr.id };
    drag = { type: hdl ? "trim-" + hdl.dataset.h : "move", tr: { ...tr }, x0: e.clientX, id: tr.id, el: aud, moved: false, pts: snapPoints(tr.id) };
  } else if (clip) {
    drag = { type: "clip", key: clip.dataset.key, x0: e.clientX, el: clip, moved: false };
    selected = { kind: "clip", id: clip.dataset.key };
  } else if (lane?.dataset.lane === "ruler") {
    drag = { type: "scrub" };
    seekTo(timeAt(e.clientX));
  } else return;
  try { e.target.setPointerCapture?.(e.pointerId); } catch (err) { /* pointeur déjà relâché */ }
  const move = (ev) => onPointerMove(ev);
  const up = (ev) => { removeEventListener("pointermove", move); removeEventListener("pointerup", up); removeEventListener("pointercancel", up); onPointerUp(ev); };
  addEventListener("pointermove", move);
  addEventListener("pointerup", up);
  addEventListener("pointercancel", up);
  if (drag.type !== "scrub") {   // la sélection se voit tout de suite, sans reconstruire le bloc touché
    root.querySelectorAll(".sel").forEach((x) => x.classList.remove("sel"));
    (drag.el.closest(".mte-row[data-tid]") || drag.el).classList.add("sel");
    renderInspector();
  }
}

function onPointerMove(e) {
  if (!drag) return;
  if (drag.type === "scrub") return seekTo(timeAt(e.clientX));
  const dx = e.clientX - drag.x0;
  if (!drag.moved && Math.abs(dx) < 5) return;
  drag.moved = true;
  const dt = dx / pps;
  if (drag.type === "clip") return dragClip(e, dx);
  const tr = drag.tr, len0 = playedLength(tr, geo.total), dur = files.get(tr.file) ?? 0;
  let patch;
  if (drag.type === "move") {
    const start = snap(Math.max(0, Math.min(geo.total - MIN_LEN, tr.start + dt)), len0, drag.pts);
    patch = { start: Math.max(0, Math.round(start * 100) / 100) };
  } else if (drag.type === "trim-r") {
    const end = snap(tr.start + len0 + dt, 0, drag.pts);
    const max = tr.loop ? Infinity : dur - tr.offset;
    patch = { length: Math.round(Math.max(MIN_LEN, Math.min(max, end - tr.start, geo.total - tr.start)) * 100) / 100 };
  } else {   // trim-l : le début avance, la fin ne bouge pas
    const t = snap(Math.max(0, tr.start + dt), 0, drag.pts);
    const lo = tr.loop ? -tr.start : Math.max(-tr.start, -tr.offset);
    const d = Math.max(lo, Math.min(len0 - MIN_LEN, t - tr.start));
    patch = { start: Math.round((tr.start + d) * 100) / 100, offset: tr.loop ? 0 : Math.round((tr.offset + d) * 100) / 100,
              length: tr.length === null ? null : Math.round(Math.max(MIN_LEN, tr.length - d) * 100) / 100 };
  }
  setTracks(tracksOf().map((t) => (t.id === tr.id ? { ...t, ...patch } : t)));
  // le bloc suit le doigt sans reconstruire la frise
  const now = tracksOf().find((t) => t.id === tr.id), l = playedLength(now, geo.total);
  drag.el.style.left = px(now.start) + "px";
  drag.el.style.width = Math.max(14, px(l)) + "px";
  const lbl = drag.el.querySelector(".lbl");
  if (lbl) lbl.textContent = `${baseName(now.file)} · ${fmt(l)}`;
  drawWaves();   // sinon l'onde, étirée avec le bloc, se déforme jusqu'au relâchement
  drag.dirty = true;
}

function dragClip(e, dx) {
  const el = drag.el, lane = el.parentElement;
  el.classList.add("lift");
  el.style.transform = `translateX(${dx}px)`;
  const mid = el.offsetLeft + el.offsetWidth / 2 + dx;
  const others = geo.blocks.filter((b) => keyOf(b.c) !== drag.key);
  let idx = others.filter((b) => px(b.start) + px(b.len) / 2 < mid).length;
  drag.idx = idx;
  const mark = lane.querySelector(".drop");
  const at = idx < others.length ? px(others[idx].start) : px(others.length ? others[others.length - 1].start + others[others.length - 1].len : 0);
  mark.hidden = false; mark.style.left = at + "px";
}

function onPointerUp() {
  const d = drag;
  drag = null;
  if (!d) return;
  if (d.type === "clip" && d.moved) {
    const kept = geo.blocks.map((b) => b.c), moved = kept.find((c) => keyOf(c) === d.key);
    const rest = kept.filter((c) => keyOf(c) !== d.key);
    rest.splice(d.idx ?? kept.indexOf(moved), 0, moved);
    return reorderKept(rest);
  }
  if (d.type === "clip") { const b = geo.blocks.find((x) => keyOf(x.c) === d.key); if (b) playhead = b.start + 0.05; showFrame(); }
  if (d.dirty) save();
  renderEditor();
}

// ------------------------------------------------------------------ tête de lecture et écoute

let player = null;   // {t0, from, timer}
let actx = null;
const voices = new Map();   // id de piste → {el, gain, file}

function seekTo(t) {
  playhead = Math.max(0, Math.min(geo.total, t));
  if (player) { player.t0 = performance.now(); player.from = playhead; syncVoices(); } else showFrame();
  renderTime();
}

/** ▶ / ❚❚ de la frise (aussi appelé par le bouton et la barre d'espace de la visionneuse, cf. commands.js). */
export function togglePreview() { player ? stopPreview() : startPreview(); }

function startPreview() {
  if (!tracksOf().length && !geo.blocks.length) { setEditorStatus("Rien à écouter : ajoute des clips ou une musique."); return; }
  if (playhead >= geo.total - 0.1) playhead = 0;
  actx = actx || new (window.AudioContext || window.webkitAudioContext)();
  actx.resume?.();
  player = { t0: performance.now(), from: playhead, timer: 0 };
  const tick = () => {
    if (!player) return;
    playhead = player.from + (performance.now() - player.t0) / 1000;
    if (playhead >= geo.total) { playhead = geo.total; stopPreview(); return renderTime(); }
    syncVoices();
    renderTime();
    const sc = root.querySelector(".mte-scroll"), x = HEAD_W + px(playhead);
    if (x > sc.scrollLeft + sc.clientWidth - 24 || x < sc.scrollLeft + HEAD_W) sc.scrollLeft = Math.max(0, x - HEAD_W - 40);
  };
  tick();
  player.timer = setInterval(tick, 50);   // pas de requestAnimationFrame : il s'arrête quand l'onglet est masqué
  root.querySelector('[data-a="play"]').textContent = "❚❚";
  $("#play").textContent = "❚❚";
}

function stopPreview(onlyId) {
  if (onlyId) {   // piste retirée
    const v = voices.get(onlyId);
    if (v) { v.el.pause(); voices.delete(onlyId); }
    return;
  }
  if (player) clearInterval(player.timer);
  player = null;
  voices.forEach((v) => v.el.pause());
  video.pause();
  video.muted = false; video.volume = 1;
  const b = root?.querySelector('[data-a="play"]');
  if (b) b.textContent = "▶";
  $("#play").textContent = "▶";
}

/** Place chaque piste à l'instant de la tête de lecture : fichier, volume (avec fondus), lecture ou pause. */
function syncVoices() {
  for (const tr of tracksOf()) {
    const len = playedLength(tr, geo.total), dur = files.get(tr.file), local = playhead - tr.start;
    let v = voices.get(tr.id);
    const usable = !tr.muted && tr.volume > 0 && dur && len > 0;
    const active = usable && local >= 0 && local < len;
    if (!active) {
      if (v) v.el.pause();
      if (!usable || local >= len || local < -4) continue;
    }
    if (!v || v.file !== tr.file) {   // créée un peu avant l'entrée de la piste : le fichier a le temps de se charger
      const el = new Audio(`/music/${encodeURIComponent(tr.file)}`);
      el.preload = "auto";
      const gain = actx.createGain();
      actx.createMediaElementSource(el).connect(gain).connect(actx.destination);
      v = { el, gain, file: tr.file, fixed: 0 };
      voices.set(tr.id, v);
    }
    let want = tr.offset + Math.max(0, local);
    if (tr.loop) want %= dur;
    if (!active) {   // en attente : positionnée au point de départ
      if (Math.abs(v.el.currentTime - want) > 0.3) v.el.currentTime = want;
      continue;
    }
    const now = performance.now();
    if (v.el.paused) {
      if (Math.abs(v.el.currentTime - want) > 0.3) v.el.currentTime = want;
      v.el.play().catch(() => setEditorStatus("⚠ lecture audio refusée par le navigateur"));
    } else if (v.el.readyState >= 3 && now - v.fixed > 1000) {   // recalage seulement quand l'audio est prêt
      const d = Math.abs(v.el.currentTime - want);
      if (d > 0.4 && !(tr.loop && d > dur - 0.4)) { v.el.currentTime = want; v.fixed = now; }
    }
    const env = Math.min(1, tr.fade_in > 0 ? local / tr.fade_in : 1, tr.fade_out > 0 ? (len - local) / tr.fade_out : 1);
    v.gain.gain.value = tr.volume * Math.max(0, env);
  }
  syncViewer(true);
}

const speedMaps = new Map();
/** Instant du clip (s depuis son début) et vitesse à t secondes après le début du clip accéléré. */
function clipTime(c, t) {
  const keys = speedKeys(c), len = c.end - c.start;
  if (!keys.length) return { src: Math.min(len, t), speed: 1 };
  const sig = keys.map((k) => `${k.t}:${k.speed}`).join(), id = keyOf(c);
  let m = speedMaps.get(id);
  if (!m || m.sig !== sig || m.len !== len) {
    const out = [0], src = [0];
    for (let u = 0; u < len; u += 1 / 30) { out.push(out[out.length - 1] + 1 / 30 / speedAt(keys, Math.min(len, u + 1 / 60))); src.push(Math.min(len, u + 1 / 30)); }
    m = { sig, len, out, src };
    speedMaps.set(id, m);
  }
  let lo = 0, hi = m.out.length - 1;
  while (lo < hi) { const mid = (lo + hi) >> 1; if (m.out[mid] < t) lo = mid + 1; else hi = mid; }
  const rel = m.src[Math.min(lo, m.src.length - 1)];
  return { src: rel, speed: speedAt(keys, rel) };
}

/** Clip sous la tête de lecture (le dernier commencé pendant un fondu enchaîné). */
function blockAt(t) {
  let found = null;
  geo.blocks.forEach((b, i) => { if (t >= b.start && t < b.start + b.len) found = { b, i }; });
  return found;
}

/** Visionneuse : affiche le clip sous la tête de lecture ; en lecture, fait jouer la vidéo
 *  (muette sur les passages accélérés) au volume du son d'origine, avec les fondus. */
function syncViewer(playing) {
  const hit = blockAt(playhead);
  if (!hit) { if (playing) video.pause(); return; }
  const { b, i } = hit, local = playhead - b.start;
  const { src, speed } = clipTime(b.c, local);
  hooks.show(b.c, src, playing);
  if (!playing) return;
  const vol = style().original_volume ?? 1, n = geo.blocks.length, t = geo.t;
  const master = Math.max(0, Math.min(1, playhead / 0.8, (geo.total - playhead) / Math.min(1.5, geo.total / 4 || 1)));
  const env = Math.min(1, i > 0 && t ? local / t : 1, (i < n - 1 || geo.card) && t ? (b.len - local) / t : 1);
  video.muted = speed > 1.05 || vol <= 0;
  video.volume = Math.min(1, vol * Math.max(0, env) * master);
}

let viewerTimer = null;
/** Pause : montre l'image du clip sous la tête de lecture (après un court délai quand on la déplace). */
function showFrame() {
  clearTimeout(viewerTimer);
  viewerTimer = setTimeout(() => { if (!player) syncViewer(false); }, 150);
}

// ------------------------------------------------------------------ démarrage

/** Branche l'éditeur sur #mte ; hooks : {reorder(clips), exclude(clip), open(clip)} fournis par montage.js. */
export function initEditor(h) {
  hooks = { ...hooks, ...h };
  root = $("#mte");
  if (!root) return;
  mount();
  onStep(() => { if (player) stopPreview(); });   // quitter l'étape arrête l'écoute
  loadFiles().then(() => renderEditor());
}
