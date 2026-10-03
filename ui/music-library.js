// Bibliothèque de musiques libres (CC BY, Kevin MacLeod) : recherche par ambiance ou mot,
// écoute d'un extrait, téléchargement et ajout comme piste audio du montage (dialogue #lib).

import { $, $$, apiOrError, esc, fmt } from "./util.js";
import { addTrack, setEditorStatus } from "./mt-editor.js";

const lib = $("#lib"), libAudio = $("#lib-audio");
let libMood = "Driving", libTimer = null;

async function libSearch() {
  const q = $("#lib-q").value.trim();
  $("#lib-list").innerHTML = "<li class='hint'>Recherche…</li>";
  const r = await apiOrError("GET", `/api/music/library?mood=${encodeURIComponent(libMood)}&q=${encodeURIComponent(q)}`);
  if (r.error) { $("#lib-list").innerHTML = `<li class="hint">⚠ ${esc(r.error)}</li>`; return; }
  $("#lib-moods").innerHTML = [["", "Toutes"], ...Object.entries(r.moods)].map(([k, v]) =>
    `<button data-mood="${k}" class="${k === libMood ? "active" : ""}">${v}</button>`).join("");
  $("#lib-list").innerHTML = r.pieces.length ? r.pieces.map((p) => `<li data-f="${esc(p.filename)}" data-src="${esc(p.preview)}">
      <button data-play title="Écouter">▶</button>
      <div><div class="t">${esc(p.title)}</div><div class="m">${fmt(p.seconds)}${p.bpm ? ` · ${p.bpm} bpm` : ""} · ${esc(p.feel)}${r.credits[p.filename] ? " · ✓ déjà téléchargée" : ""}</div>
        <div class="d" title="${esc(p.description)}">${esc(p.description || p.instruments)}</div></div>
      <button data-use class="primary">Utiliser</button></li>`).join("") : "<li class='hint'>Aucun résultat.</li>";
}

document.addEventListener("click", (e) => { if (e.target.closest("#lib-open")) { lib.showModal(); libSearch(); } });
lib.addEventListener("close", () => libAudio.pause());
lib.addEventListener("click", async (e) => {
  if (e.target === lib || e.target.closest("[data-close]")) return lib.close();
  const mood = e.target.closest("[data-mood]");
  if (mood) { libMood = mood.dataset.mood; return libSearch(); }
  const li = e.target.closest("#lib-list li");
  if (!li) return;
  if (e.target.closest("[data-play]")) {   // écoute : un seul extrait à la fois, second clic = pause
    const same = libAudio.src === li.dataset.src && !libAudio.paused;
    $$("#lib-list li").forEach((x) => { x.classList.remove("playing"); x.querySelector("[data-play]") && (x.querySelector("[data-play]").textContent = "▶"); });
    if (same) { libAudio.pause(); return; }
    libAudio.src = li.dataset.src; libAudio.play();
    li.classList.add("playing"); e.target.textContent = "❚❚";
  }
  if (e.target.closest("[data-use]")) {
    e.target.disabled = true; e.target.textContent = "Téléchargement…";
    const r = await apiOrError("POST", "/api/music/library", { filename: li.dataset.f });
    if (r.error) { e.target.textContent = "⚠ échec"; setEditorStatus("⚠ " + r.error); return; }
    libAudio.pause(); lib.close();
    await addTrack(r.name);
    setEditorStatus(`✓ ${r.name} ajoutée — crédit ajouté à la fin du montage`);
  }
});
$("#lib-q").addEventListener("input", () => { clearTimeout(libTimer); libTimer = setTimeout(libSearch, 350); });
