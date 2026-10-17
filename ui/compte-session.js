// Session du compte côté page : les appels au service passent par ici. Quand le jeton d'accès a
// expiré, on en demande un nouveau une fois ; si cela échoue, retour à la page de connexion.

/** fetch vers le service ; ne revient avec un 401 que si la session ne peut pas être prolongée. */
export async function authFetch(url, init) {
  let r = await fetch(url, init);
  if (r.status !== 401) return r;
  const again = await fetch("/api/compte/rafraichir", { method: "POST" });
  if (again.ok) r = await fetch(url, init);
  if (r.status === 401) location.href = "compte.html?next=" + encodeURIComponent(location.pathname.split("/").pop());
  return r;
}

/** Affiche le compte connecté et le bouton de déconnexion dans l'élément donné (rien si les comptes sont désactivés). */
export async function showAccount(el) {
  const s = await fetch("/api/compte").then((r) => r.json()).catch(() => null);
  if (!s || !s.auth || !s.signed_in) return;
  el.innerHTML = `<span class="muted"></span> <button>Déconnexion</button>`;
  el.querySelector("span").textContent = s.email || "";
  el.querySelector("button").addEventListener("click", async () => {
    await fetch("/api/compte/deconnexion", { method: "POST" });
    location.href = "compte.html";
  });
}
