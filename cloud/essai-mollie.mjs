// Pilotage du faux serveur Mollie (cloud/faux-mollie.mjs) par les essais : ce que ferait le client
// sur la page de paiement, et ce que Mollie ferait de lui-même (échéance, contestation).
export const faux = process.env.BIKE360_MOLLIE_API || "http://127.0.0.1:12112";

export async function pilot(path, body = {}) {
  const r = await fetch(`${faux}/_faux/${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  return r.json();
}
/** État complet du faux Mollie : clients, paiements, mandats, abonnements, appels reçus. */
export const mollie = async () => (await fetch(`${faux}/_faux/etat`)).json();
/** Identifiant du paiement d'une page de paiement. */
export const paymentOf = (url) => url.split("/").pop();
/** Le client paie (ou échoue : {status: "failed"}) sur la page de paiement ; Mollie notifie le service. */
export const pay = (url, body) => pilot(`payer/${paymentOf(url)}`, body);
/** Abonnements d'un compte chez le faux Mollie, retrouvés par le compte noté sur son client. */
export async function subscriptionsOf(sub) {
  const m = await mollie();
  const customers = Object.values(m.customers).filter((c) => c.metadata?.client === sub).map((c) => c.id);
  return Object.values(m.subscriptions).filter((s) => customers.includes(s.customerId));
}
