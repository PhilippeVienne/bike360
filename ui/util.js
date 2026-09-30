// Utilitaires communs : sélecteur DOM, appels au serveur, formats d'heure et de durée,
// échappement HTML, couleurs du thème, stockage local du navigateur.

import { st } from "./state.js";

export const $ = (s) => document.querySelector(s);
export const $$ = (s) => document.querySelectorAll(s);

/** Appel JSON au serveur ; lève une erreur si le statut HTTP n'est pas 2xx. */
export async function api(method, url, body) {
  const r = await fetch(url, { method, headers: { "Content-Type": "application/json" },
                               body: body === undefined ? undefined : JSON.stringify(body) });
  if (!r.ok) throw new Error(`${method} ${url} → ${r.status}`);
  return r.json();
}

/** Appel JSON dont on lit la réponse même en erreur : le serveur y met alors {error: "…"}. */
export function apiOrError(method, url, body) {
  return fetch(url, { method, headers: { "Content-Type": "application/json" },
                      body: body === undefined ? undefined : JSON.stringify(body) }).then((r) => r.json());
}

// ------------------------------------------------------------------ formats

/** Durée « h:mm:ss » ou « m:ss ». */
export const fmt = (t) => {
  t = Math.max(0, Math.floor(t));
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
  return (h ? h + ":" + String(m).padStart(2, "0") : m) + ":" + String(s).padStart(2, "0");
};
/** Heure locale « hh:mm:ss » de l'instant t de la session affichée. */
export const localClock = (t) => {
  const s0 = +st.s.time.slice(0, 2) * 3600 + +st.s.time.slice(2, 4) * 60 + +st.s.time.slice(4, 6);
  const x = Math.floor(s0 + t) % 86400;
  return [x / 3600 | 0, (x % 3600) / 60 | 0, x % 60].map((v) => String(v).padStart(2, "0")).join(":");
};
/** Heure locale au dixième de seconde (bords de clip). */
export const fmtPrecise = (t) => localClock(t) + "." + Math.floor((t % 1) * 10);
/** « samedi 29 août 2026 » depuis « 20260829 ». */
export const dayLabel = (d) => new Date(`${d.slice(0, 4)}-${d.slice(4, 6)}-${d.slice(6)}T12:00`)
  .toLocaleDateString("fr-FR", { weekday: "long", day: "numeric", month: "long", year: "numeric" });
/** Heure de début « hh:mm » d'une session. */
export const hhmm = (s) => `${s.time.slice(0, 2)}:${s.time.slice(2, 4)}`;

export const esc = (x) => String(x).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

/** Lien vers un fichier exporté. */
export const exportLink = (name) => `✓ <a href="/exports/${encodeURIComponent(name)}" target="_blank">${name}</a>`;

/** Valeur d'une variable CSS du thème (--accent…). */
export const css = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();

// ------------------------------------------------------------------ stockage local (préférences de ce navigateur)

export function storageGet(key) {
  try { return localStorage.getItem(key); } catch (e) { return null; }   // stockage indisponible
}
export function storageSet(key, value) {
  try { localStorage.setItem(key, value); } catch (e) { /* stockage indisponible */ }
}
