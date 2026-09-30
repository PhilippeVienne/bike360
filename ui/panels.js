// Disposition : largeur des panneaux latéraux (poignées à glisser, mémorisées dans ce
// navigateur), panneau Projet masquable (☰) ou en tiroir sur téléphone, onglets
// Fichiers / Montage.
// Partage : isNarrow, closeDrawer.

import { $, $$, storageGet, storageSet } from "./util.js";
import { refreshMapSize } from "./map.js";
import { refreshSessions } from "./project.js";
import { pollMontage } from "./montage.js";

/** Écran étroit (téléphone) : même seuil que les @media de style.css. */
export const isNarrow = () => matchMedia("(max-width: 820px)").matches;

// ------------------------------------------------------------------ poignées de redimensionnement
{
  const main = $("main");
  const projectW = () => $("#project").getBoundingClientRect().width;
  const asideW = () => $("main > aside").getBoundingClientRect().width;
  const place = () => {   // les poignées suivent le bord des panneaux
    $("#split-left").style.left = projectW() + "px";
    $("#split-right").style.left = (main.clientWidth - asideW()) + "px";
  };
  const setW = (name, px) => { main.style.setProperty(name, Math.round(px) + "px"); place(); };
  // largeurs mémorisées : {project, aside} en px
  const loadWidths = () => { try { return JSON.parse(storageGet("panelWidths") || "{}"); } catch (e) { return {}; } };
  const storeWidth = (key, px) => {
    const saved = loadWidths();
    if (px === undefined) delete saved[key]; else saved[key] = px;
    storageSet("panelWidths", JSON.stringify(saved));
  };
  const saved = loadWidths();
  if (saved.project) setW("--project-w", saved.project);
  if (saved.aside) setW("--aside-w", saved.aside);

  for (const [id, name, side] of [["#split-left", "--project-w", "left"], ["#split-right", "--aside-w", "right"]]) {
    const el = $(id), key = side === "left" ? "project" : "aside";
    el.addEventListener("pointerdown", (e) => {
      el.setPointerCapture(e.pointerId);
      el.classList.add("dragging");
      const move = (ev) => {
        const r = main.getBoundingClientRect(), W = r.width;
        let px = side === "left" ? ev.clientX - r.left : r.right - ev.clientX;
        const other = side === "left" ? asideW() : projectW();
        px = Math.max(side === "left" ? 180 : 240, Math.min(px, W - other - 420));   // la vidéo garde ≥ 420 px
        setW(name, px);
      };
      const up = (ev) => {
        if (ev && ev.type === "pointerup") move(ev);   // position finale, même sans dernier « move »
        el.classList.remove("dragging");
        el.removeEventListener("pointermove", move);
        storeWidth(key, parseInt(main.style.getPropertyValue(name)));
        refreshMapSize();
      };
      el.addEventListener("pointermove", move);
      el.addEventListener("pointerup", up, { once: true });
      el.addEventListener("pointercancel", up, { once: true });
    });
    el.addEventListener("dblclick", () => {   // double-clic : largeur par défaut
      main.style.removeProperty(name); place();
      storeWidth(key, undefined);
    });
  }
  new ResizeObserver(place).observe(main);
  new ResizeObserver(place).observe($("#project"));
}

// ------------------------------------------------------------------ panneau Projet : masqué (☰) ou tiroir (téléphone)

export const closeDrawer = () => document.body.classList.remove("project-open");
$("#drawer-backdrop").addEventListener("click", closeDrawer);
$("#project-toggle").addEventListener("click", () => {
  if (isNarrow()) { document.body.classList.toggle("project-open"); return; }   // téléphone : tiroir
  document.body.classList.toggle("no-project");
  storageSet("noProject", document.body.classList.contains("no-project") ? "1" : "");
});
if (storageGet("noProject")) document.body.classList.add("no-project");

// onglets Fichiers / Montage
$$("[data-ptab]").forEach((b) => b.addEventListener("click", () => {
  $$("[data-ptab]").forEach((x) => x.classList.toggle("active", x === b));
  $("#p-files").hidden = b.dataset.ptab !== "files";
  $("#p-montage").hidden = b.dataset.ptab !== "montage";
  if (b.dataset.ptab === "montage") { refreshSessions(); pollMontage(); }
}));
