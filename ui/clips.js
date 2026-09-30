// Clips de la session affichée : création (I/O, clip rapide), sélection, bords, suppression
// (un clip ou les clips cochés), liste du panneau de droite et enregistrement sur le serveur.
// Partage : saveClips, saveClipsNow, renderClips, selectClip, setClipEdge, editEdge,
// deleteSelected, markIn, markOut, quickClip, updateMarkUI.

import { st, now, QUICK_CLIP } from "./state.js";
import { $, api, fmt, fmtPrecise, localClock } from "./util.js";
import { clipKeys, clipMode } from "./geometry.js";
import { currentViewParams, setView } from "./view.js";
import { seek } from "./playback.js";
import { drawClipsOnMap } from "./map.js";
import { renderEditor } from "./clip-editor.js";
import { renderStats } from "./stats.js";
import { refreshSessions } from "./project.js";
import { updateMontageMap } from "./montage.js";

// ------------------------------------------------------------------ enregistrement

/** Trie les clips, rafraîchit l'affichage et enregistre (regroupé sur 300 ms).
 *  keep : clip modifié, qui reste sélectionné après le tri. */
let saveTimer = null;
export function saveClips(keep) {
  if (keep && keep.auto) delete keep.auto;   // clip auto retouché : il devient le tien (plus remplacé)
  const sel = st.clips[st.sel];
  st.clips.sort((a, b) => a.start - b.start);
  st.sel = sel ? st.clips.indexOf(keep ?? sel) : null;
  renderClips(); drawClipsOnMap();
  clearTimeout(saveTimer);
  const sid = st.s.id, clips = st.clips.map((c) => ({ ...c }));
  saveTimer = setTimeout(() => api("PUT", `/api/selections/${sid}`, clips).then(refreshSessions).catch(console.error), 300);
}

/** Enregistre tout de suite (avant un export), en annulant l'enregistrement différé. */
export async function saveClipsNow() {
  clearTimeout(saveTimer);
  await api("PUT", `/api/selections/${st.s.id}`, st.clips);
}

// ------------------------------------------------------------------ création

function addClip(start, end) {
  start = Math.max(0, start); end = Math.min(st.s.duration, end);
  if (end - start < 0.5) return;
  // identifiant stable : ordre du montage (crypto.randomUUID indisponible en http sur le réseau local)
  const id = Math.random().toString(16).slice(2, 10);
  const c = { id, start: +start.toFixed(2), end: +end.toFixed(2), ...currentViewParams() };
  st.clips.push(c);
  st.sel = st.clips.length - 1;
  saveClips(c);
}

export function markIn() { st.inPoint = now(); updateMarkUI(); }
export function markOut() {
  const t = now();
  if (st.inPoint === null) { $("#mark-hint").textContent = "Pose d'abord un début (I)"; return; }
  const [a, b] = [Math.min(st.inPoint, t), Math.max(st.inPoint, t)];
  st.inPoint = null;
  addClip(a, b);
  updateMarkUI();
}
export function quickClip() { const t = now(); addClip(t - QUICK_CLIP[0], t + QUICK_CLIP[1]); }

export function updateMarkUI() {
  $("#mark-in").classList.toggle("active", st.inPoint !== null);
  $("#mark-hint").textContent = st.inPoint !== null ? `début ${fmtPrecise(st.inPoint)} → O pour la fin` : "";
}

$("#mark-in").addEventListener("click", markIn);
$("#mark-out").addEventListener("click", markOut);
$("#quick-clip").addEventListener("click", quickClip);

// ------------------------------------------------------------------ sélection et bords

/** Sélectionne le clip k ; go = "seek" pour s'y placer avec son cadrage, "play" pour le lire. */
export function selectClip(k, go) {
  st.sel = k;
  const c = st.clips[k];
  if (c && go) {
    setView({ yaw: c.yaw, pitch: c.pitch, fov: c.fov, horizon: clipMode(c), raw: false });
    seek(c.start, go === "play" ? true : undefined);
  }
  renderClips();
}

/** Déplace un bord du clip (0,5 s minimum, dans la session), sans enregistrer. */
export function setClipEdge(c, edge, value) {
  if (edge === "start") c.start = +Math.max(0, Math.min(value, c.end - 0.5)).toFixed(2);
  else c.end = +Math.min(st.s.duration, Math.max(value, c.start + 0.5)).toFixed(2);
}

/** Déplace un bord du clip sélectionné et enregistre. */
export function editEdge(edge, value) {
  const c = st.clips[st.sel];
  if (!c) return;
  setClipEdge(c, edge, value);
  saveClips(c);
}

// ------------------------------------------------------------------ suppression

export function deleteSelected() {
  if (st.sel === null || !st.clips[st.sel]) return;
  st.clips.splice(st.sel, 1);
  st.sel = null;
  saveClips();
}

// Suppression en masse : premier clic arme le bouton, second clic (dans les 3 s) confirme.
let bulkArmed = null;
function updateBulk() {
  for (const c of st.checked) if (!st.clips.includes(c)) st.checked.delete(c);
  const n = st.checked.size, b = $("#delete-checked");
  b.disabled = n === 0;
  b.textContent = bulkArmed ? `Confirmer (${n}) ?` : n ? `Supprimer ${n}` : "Supprimer";
  const all = $("#check-all");
  all.checked = n > 0 && n === st.clips.length;
  all.indeterminate = n > 0 && n < st.clips.length;
}
$("#check-all").addEventListener("change", (e) => {
  st.checked = e.target.checked ? new Set(st.clips) : new Set();
  renderClips();
});
$("#delete-checked").addEventListener("click", () => {
  if (!bulkArmed) {
    bulkArmed = setTimeout(() => { bulkArmed = null; updateBulk(); }, 3000);
    updateBulk();
    return;
  }
  clearTimeout(bulkArmed); bulkArmed = null;
  const sel = st.clips[st.sel];
  st.clips = st.clips.filter((c) => !st.checked.has(c));
  st.checked.clear();
  st.sel = sel && st.clips.includes(sel) ? st.clips.indexOf(sel) : null;
  saveClips();
});

// ------------------------------------------------------------------ liste

/** Liste des clips (panneau de droite), puis éditeur, stats et mini-carte du montage. */
export function renderClips() {
  const ol = $("#clips");
  ol.innerHTML = "";
  const t = now();
  st.clips.forEach((c, k) => {
    const li = document.createElement("li");
    li.classList.toggle("current", t >= c.start && t <= c.end);
    li.classList.toggle("selected", k === st.sel);
    li.innerHTML = `<input type="checkbox" class="chk" ${st.checked.has(c) ? "checked" : ""}>
      <span class="n">${k + 1}</span>
      <div><div>${localClock(c.start)} → ${localClock(c.end)} <span class="muted">(${fmt(c.end - c.start)})</span>${c.auto ? '<span class="badge auto" title="Créé par le montage automatique">auto</span>' : ""}</div>
      <div class="meta">yaw ${c.yaw}° · pitch ${c.pitch}° · champ ${c.fov}° · horizon ${clipMode(c)}${clipKeys(c).length ? ` · ◆ ${clipKeys(c).length}` : ""}</div></div>
      <div class="act"><button data-a="play" title="Lire le clip">▶</button></div>`;
    li.addEventListener("click", (e) => {
      if (e.target.classList.contains("chk")) {
        e.target.checked ? st.checked.add(c) : st.checked.delete(c);
        updateBulk();
        return;
      }
      selectClip(k, e.target.dataset.a === "play" ? "play" : "seek");
    });
    ol.appendChild(li);
  });
  const total = st.clips.reduce((s, c) => s + c.end - c.start, 0);
  $("#clips-total").textContent = st.clips.length ? `${st.clips.length} · ${fmt(total)}` : "aucun";
  if (!st.clips.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.innerHTML = `Aucun clip pour l'instant.<br>Pendant la lecture : <kbd>I</kbd> puis <kbd>O</kbd> pour poser début et fin,
      ou <kbd>C</kbd> pour un clip de 15 s. <kbd>N</kbd> saute au prochain moment fort (ambre sur la frise).`;
    ol.appendChild(li);
  }
  renderEditor();
  updateBulk();
  renderStats();
  updateMontageMap();
}
