// Page du compte : connexion, inscription, confirmation de l'adresse par le code reçu par courriel,
// et mot de passe oublié (un code reçu par courriel permet d'en choisir un nouveau).
// Les jetons ne passent jamais par ce code : le service les pose dans des témoins que la page ne lit pas.

const $ = (s) => document.querySelector(s);
const next = () => {
  const n = new URLSearchParams(location.search).get("next") || "bibliotheque.html";
  return /^[a-z-]+\.html$/.test(n) ? n : "bibliotheque.html";   // jamais d'adresse extérieure
};

// connexion | inscription | confirmation | oubli | reinitialisation ; le site vitrine mène droit à l'inscription
let mode = new URLSearchParams(location.search).has("inscription") ? "inscription" : "connexion";
let waiting = null;       // attente de la confirmation de l'adresse : minuterie
const WAIT_EVERY_MS = 3000, WAIT_TRIES = 60;

/** Compte créé mais pas encore confirmé : on se connecte dès que l'adresse l'est, où qu'elle l'ait été
 *  (code saisi ici, lien ouvert sur un autre appareil, confirmation par l'administration). */
function waitForConfirmation(email, password, tries = WAIT_TRIES) {
  clearTimeout(waiting);
  if (mode !== "confirmation" || tries <= 0) return;
  waiting = setTimeout(async () => {
    const r = await post("connexion", { email, password });
    if (r.ok) { location.href = next(); return; }
    if (r.status === 403) waitForConfirmation(email, password, tries - 1);   // toujours pas confirmée
  }, WAIT_EVERY_MS);
}

async function post(path, body) {
  const r = await fetch(`/api/compte/${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const j = await r.json().catch(() => ({}));
  return { ok: r.ok, status: r.status, error: j.error, ...j };
}

function render(message = "") {
  $("#title").textContent = { connexion: "Se connecter", inscription: "Créer un compte", confirmation: "Confirmer mon adresse",
                              oubli: "Mot de passe oublié", reinitialisation: "Nouveau mot de passe" }[mode];
  $("#password-row").hidden = mode === "confirmation" || mode === "oubli";
  $("#password-row").firstChild.textContent = mode === "reinitialisation" ? "Nouveau mot de passe " : "Mot de passe ";
  $("#code-row").hidden = mode !== "confirmation" && mode !== "reinitialisation";
  $("#password").autocomplete = mode === "connexion" ? "current-password" : "new-password";
  $("#submit").textContent = { connexion: "Se connecter", inscription: "Créer mon compte", confirmation: "Confirmer",
                               oubli: "Recevoir un code", reinitialisation: "Changer mon mot de passe" }[mode];
  $("#switch").textContent = mode === "connexion" ? "Pas encore de compte ? En créer un" : "J'ai déjà un compte";
  $("#forgot").hidden = mode !== "connexion";
  $("#message").textContent = message;
}

$("#form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const email = $("#email").value.trim(), password = $("#password").value, code = $("#code").value.trim();
  $("#submit").disabled = true;
  if (mode === "inscription") {
    const r = await post("inscription", { email, password });
    if (r.ok) {
      mode = r.confirmed ? "connexion" : "confirmation";
      render(r.confirmed ? "Compte créé." : "Un code t'a été envoyé par courriel. La connexion se fera seule dès que l'adresse sera confirmée.");
      waitForConfirmation(email, password);
    }
    else render("⚠ " + (r.error || "inscription impossible"));
  } else if (mode === "confirmation") {
    const r = await post("confirmation", { email, code });
    if (r.ok && password) {   // le mot de passe est encore dans le formulaire : connexion directe
      const s = await post("connexion", { email, password });
      if (s.ok) { location.href = next(); return; }
    }
    if (r.ok) { mode = "connexion"; render("Adresse confirmée : tu peux te connecter."); }
    else render("⚠ " + (r.error || "confirmation impossible"));
  } else if (mode === "oubli") {
    const r = await post("oubli", { email });
    if (r.ok) { mode = "reinitialisation"; $("#password").value = ""; render("Si un compte existe à cette adresse, un code vient de lui être envoyé."); }
    else render("⚠ " + (r.error || "demande impossible"));
  } else if (mode === "reinitialisation") {
    const r = await post("reinitialisation", { email, code, password });
    if (r.ok) {
      const s = await post("connexion", { email, password });
      if (s.ok) { location.href = next(); return; }
      mode = "connexion"; render("Mot de passe changé : tu peux te connecter.");
    }
    else render("⚠ " + (r.error || "changement impossible"));
  } else {
    const r = await post("connexion", { email, password });
    if (r.ok) { location.href = next(); return; }
    if (r.status === 403) mode = "confirmation";   // compte créé mais pas encore confirmé
    render("⚠ " + (r.error || "connexion impossible"));
  }
  $("#submit").disabled = false;
});

$("#switch").addEventListener("click", () => { clearTimeout(waiting); mode = mode === "connexion" ? "inscription" : "connexion"; render(); });
$("#forgot").addEventListener("click", () => { clearTimeout(waiting); mode = "oubli"; render(); });

// déjà connecté, ou comptes désactivés sur ce service : rien à faire ici
fetch("/api/compte").then((r) => r.json()).then((s) => { if (s.signed_in) location.href = next(); }).catch(() => {});
render();
