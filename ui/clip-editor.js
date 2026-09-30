// Éditeur du clip sélectionné (panneau de droite) : bords au dixième, cadrage, horizon,
// points clés (liste, courbes de transition), suivi d'un compagnon, lecture, boucle.
// Partage : renderEditor, jumpKey.

import { st, now } from "./state.js";
import { $, fmt, fmtPrecise } from "./util.js";
import { CURVE_LABELS, clipKeys, clipMode } from "./geometry.js";
import { activeClip, currentViewParams, setView, upsertKey } from "./view.js";
import { seek } from "./playback.js";
import { deleteSelected, editEdge, renderClips, saveClips, selectClip } from "./clips.js";
import { setZoneEdit } from "./zones.js";
import { SPEEDS, outputDuration, speedKeys } from "./speed.js";

export function renderEditor() {
  const c = st.clips[st.sel], ed = $("#clip-editor");
  ed.hidden = !c;
  if (!c) return;
  $("#ed-title").textContent = `Clip ${st.sel + 1} · ${fmt(c.end - c.start)}`;
  $("#ed-start").textContent = fmtPrecise(c.start);
  $("#ed-end").textContent = fmtPrecise(c.end);
  $("#ed-view").textContent = `yaw ${c.yaw}° · pitch ${c.pitch}° · champ ${c.fov}°`;
  $("#ed-horizon").value = clipMode(c);
  $("#auto-key").checked = st.autoKey;
  renderKeyList(c);
  renderSpeedList(c);
  $("#ed-loop").classList.toggle("active", st.loop);
}

/** Liste des points clés du clip : aller à, courbe vers le point suivant, suppression. */
function renderKeyList(c) {
  const ol = $("#ed-keylist"), keys = clipKeys(c);
  ol.innerHTML = keys.length ? "" : `<li class="muted">Aucun point : cadrage fixe du clip. Place la lecture, cadre, puis ◆.</li>`;
  keys.forEach((k, i) => {
    const li = document.createElement("li");
    li.classList.toggle("here", Math.abs(now() - c.start - k.t) < 0.05);
    const last = i === keys.length - 1;
    li.innerHTML = `<button data-k="go" title="Aller à ce point">◆ ${fmt(c.start + k.t)}.${Math.floor((k.t % 1) * 10)}</button>
      <span class="muted">${k.yaw.toFixed(0)}° / ${k.pitch.toFixed(0)}° / rot ${k.roll.toFixed(0)}° / ${k.fov.toFixed(0)}°</span>
      <span>${last ? "" : `<select data-k="curve" title="Transition vers le point suivant">${Object.entries(CURVE_LABELS)
        .map(([v, l]) => `<option value="${v}" ${v === (k.curve || "linear") ? "selected" : ""}>${l}</option>`).join("")}</select>`}
      <button data-k="del" title="Supprimer ce point">✕</button></span>`;
    li.addEventListener("click", (e) => {
      const what = e.target.dataset.k;
      if (what === "go") { st.viewOverride = false; seek(c.start + k.t); }
      else if (what === "del") { c.keyframes = keys.filter((x) => x !== k); delete c.roll_keys; saveClips(c); }
    });
    li.querySelector("select")?.addEventListener("change", (e) => {
      c.keyframes = keys; delete c.roll_keys;
      k.curve = e.target.value; saveClips(c);
    });
    ol.appendChild(li);
  });
}

/** Points de vitesse du clip : aller à, facteur, suppression ; durée une fois accéléré. */
function renderSpeedList(c) {
  const ol = $("#ed-speedlist"), keys = speedKeys(c);
  const out = outputDuration(c);
  $("#ed-speed-out").textContent = keys.length ? `${fmt(c.end - c.start)} → ${fmt(out)} à l'export` : "vitesse normale";
  ol.innerHTML = "";
  keys.forEach((k) => {
    const li = document.createElement("li");
    li.classList.toggle("here", Math.abs(now() - c.start - k.t) < 0.05);
    li.innerHTML = `<button data-s="go" title="Aller à ce point">⏩ ${fmt(c.start + k.t)}.${Math.floor((k.t % 1) * 10)}</button>
      <select data-s="speed" title="Vitesse à ce point">${SPEEDS.map((v) => `<option value="${v}" ${v === k.speed ? "selected" : ""}>×${v}</option>`).join("")}</select>
      <button data-s="del" title="Supprimer ce point">✕</button>`;
    li.addEventListener("click", (e) => {
      const what = e.target.dataset.s;
      if (what === "go") seek(c.start + k.t);
      else if (what === "del") { c.speed_keys = keys.filter((x) => x !== k); saveClips(c); }
    });
    li.querySelector("select").addEventListener("change", (e) => { k.speed = +e.target.value; c.speed_keys = keys; saveClips(c); });
    ol.appendChild(li);
  });
}

/** Pose (ou remplace) un point de vitesse à la tête de lecture, dans le clip. */
function addSpeedKey(c, speed) {
  const t = Math.round(Math.min(c.end - c.start, Math.max(0, now() - c.start)) * 10) / 10;
  c.speed_keys = [...speedKeys(c).filter((k) => Math.abs(k.t - t) > 0.05), { t, speed }].sort((a, b) => a.t - b.t);
  saveClips(c);
}

/** Saute au point clé suivant (dir > 0) ou précédent du clip sélectionné ou traversé. */
export function jumpKey(dir) {
  const c = st.clips[st.sel] || activeClip(now());
  if (!c) return;
  const t = now() - c.start, keys = clipKeys(c).map((k) => k.t);
  const target = dir > 0 ? keys.find((x) => x > t + 0.05) : keys.filter((x) => x < t - 0.05).pop();
  if (target !== undefined) { st.viewOverride = false; seek(c.start + target); }
}

// boutons de l'éditeur (data-edge + data-d : bords ; data-a : actions)
$("#clip-editor").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  const c = st.clips[st.sel];
  if (!b || !c) return;
  const { edge, d, a } = b.dataset;
  if (edge && d === "here") editEdge(edge, now());
  else if (edge) editEdge(edge, c[edge] + +d);
  else if (a === "view") { Object.assign(c, currentViewParams(), { horizon: clipMode(c) }); saveClips(c); }
  else if (a === "clear-keys") { delete c.roll_keys; delete c.keyframes; saveClips(c); }
  else if (a === "follow") {
    if (now() < c.start || now() > c.end) seek(c.start + Math.min(2, (c.end - c.start) / 2), false);
    setZoneEdit(true, "follow");
  }
  else if (a === "add-key") upsertKey(c, now());
  else if (a === "add-speed") addSpeedKey(c, +$("#ed-speed").value);
  else if (a === "play") selectClip(st.sel, "play");
  else if (a === "goto-end") seek(Math.max(c.start, c.end - 3), true);
  else if (a === "loop") { st.loop = !st.loop; renderEditor(); }
  else if (a === "del") deleteSelected();
  else if (a === "close") { st.sel = null; renderClips(); }
});
$("#ed-horizon").addEventListener("change", (e) => {
  const c = st.clips[st.sel];
  if (!c) return;
  c.horizon = e.target.value;
  delete c.level;
  setView({ horizon: c.horizon });
  saveClips(c);
});
$("#auto-key").addEventListener("change", (e) => (st.autoKey = e.target.checked));
$("#ed-speed").innerHTML = SPEEDS.map((v) => `<option value="${v}" ${v === 4 ? "selected" : ""}>×${v}</option>`).join("");
