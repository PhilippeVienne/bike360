// Mise en page ordinateur (index.html) : largeur du panneau de droite de l'étape ② (poignée
// à glisser, mémorisée dans ce navigateur, double-clic = largeur par défaut).
// Le reste (étapes, onglets, dialogues) est commun aux deux interfaces.

import { $, storageGet, storageSet } from "./util.js";
import { refreshMapSize } from "./map.js";

const main = $("main"), split = $("#split-right"), side = $("#side");
const place = () => { split.style.left = (main.clientWidth - side.getBoundingClientRect().width) + "px"; };
const setW = (px) => { main.style.setProperty("--aside-w", Math.round(px) + "px"); place(); };
const saved = +storageGet("bike360.asideW");
if (saved) setW(saved);

split.addEventListener("pointerdown", (e) => {
  split.setPointerCapture(e.pointerId);
  split.classList.add("dragging");
  const move = (ev) => {
    const r = main.getBoundingClientRect();
    setW(Math.max(260, Math.min(r.right - ev.clientX, r.width - 420)));   // la vidéo garde ≥ 420 px
  };
  const up = (ev) => {
    if (ev && ev.type === "pointerup") move(ev);   // position finale, même sans dernier « move »
    split.classList.remove("dragging");
    split.removeEventListener("pointermove", move);
    storageSet("bike360.asideW", parseInt(main.style.getPropertyValue("--aside-w")) || "");
    refreshMapSize();
  };
  split.addEventListener("pointermove", move);
  split.addEventListener("pointerup", up, { once: true });
  split.addEventListener("pointercancel", up, { once: true });
});
split.addEventListener("dblclick", () => {
  main.style.removeProperty("--aside-w"); place();
  storageSet("bike360.asideW", "");
});
new ResizeObserver(place).observe(main);
new ResizeObserver(place).observe(side);
