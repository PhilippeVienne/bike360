// Faux serveur Mollie pour les essais locaux : il répond comme l'API v2 de Mollie pour les appels
// dont le service se sert (clients, paiements, mandats, abonnements), garde son état en mémoire et
// envoie les notifications comme Mollie : un formulaire qui ne porte que l'identifiant du paiement.
// Aucun appel ne sort de la machine, aucun paiement n'a lieu.
//
//   node cloud/faux-mollie.mjs [PORT]        (12112 par défaut, sur 127.0.0.1 seulement)
//
// Écrit d'après https://docs.mollie.com/reference ; ce que la documentation ne précise pas est
// marqué « supposé » ci-dessous, et reste à confirmer avec une clé d'essai de Mollie.
//
// La page de paiement (`_links.checkout.href`) est une page de ce serveur, avec trois boutons.
// Routes de pilotage, qui n'existent pas chez Mollie (préfixe /_faux) :
//   POST /_faux/payer/<tr_…>      {status, method}  termine un paiement ouvert (paid par défaut, par carte)
//   POST /_faux/echeance/<sub_…>  {status}          Mollie prélève une échéance de l'abonnement
//   POST /_faux/reprise/<tr_…>    {type}            contestation (chargeback) ou remboursement complet (refund)
//   POST /_faux/notifier/<tr_…>                     représente la notification d'un paiement
//   POST /_faux/panne             {calls}           les prochains appels à l'API répondent 503
//   GET  /_faux/etat                                tout l'état, et les appels reçus
import { createServer } from "node:http";
import { randomBytes } from "node:crypto";

const port = Number(process.argv[2] || 12112);
const origin = `http://127.0.0.1:${port}`;
const db = { customers: {}, payments: {}, mandates: {}, subscriptions: {}, calls: [], notices: [] };
const replays = new Map();   // clé d'idempotence → {signature, status, body}
let outage = 0;

const id = (prefix) => `${prefix}_${randomBytes(6).toString("hex")}`;
const now = () => new Date().toISOString().replace(/\.\d+Z$/, "+00:00");
const today = () => new Date().toISOString().slice(0, 10);
const link = (href, type = "application/hal+json") => ({ href, type });
const fail = (status, title, detail, field) => ({ status, body: { status, title, detail, ...(field ? { field } : {}),
  _links: { documentation: link("https://docs.mollie.com/overview/handling-errors", "text/html") } } });
const missing = () => fail(404, "Not Found", "No entity with this ID exists.");
const invalid = (detail, field) => fail(422, "Unprocessable Entity", detail, field);
const money = (a) => a && a.currency === "EUR" && typeof a.value === "string" && /^\d+\.\d{2}$/.test(a.value);

/** Ajoute un intervalle de Mollie (« 12 months », « 2 weeks », « 30 days ») à une date AAAA-MM-JJ. */
function later(date, interval) {
  const [n, unit] = interval.split(" ");
  const d = new Date(`${date}T00:00:00Z`);
  if (unit.startsWith("month")) d.setUTCMonth(d.getUTCMonth() + Number(n));
  else d.setUTCDate(d.getUTCDate() + Number(n) * (unit.startsWith("week") ? 7 : 1));
  return d.toISOString().slice(0, 10);
}

/** Envoie la notification d'un paiement, comme Mollie : POST d'un formulaire, champ `id`. */
async function notify(payment) {
  if (!payment.webhookUrl) return null;
  let status = 0;
  try {
    const r = await fetch(payment.webhookUrl, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" },
                                                body: new URLSearchParams({ id: payment.id }), signal: AbortSignal.timeout(15000) });
    status = r.status;
  } catch { /* service injoignable : Mollie représenterait la notification */ }
  db.notices.push({ id: payment.id, status });
  return status;
}

function view(payment) {
  const self = `${origin}/v2/payments/${payment.id}`;
  return { resource: "payment", mode: "test", profileId: "pfl_faux", ...payment,
    _links: { self: link(self), ...(payment.status === "open" ? { checkout: link(`${origin}/checkout/${payment.id}`, "text/html") } : {}),
              documentation: link("https://docs.mollie.com/reference/get-payment", "text/html") } };
}

/** Mandat que crée un premier paiement abouti : par carte, ou par prélèvement pour les virements. */
function mandateFor(payment) {
  const card = payment.method === "creditcard";
  const mandate = { resource: "mandate", id: id("mdt"), mode: "test", status: "valid", method: card ? "creditcard" : payment.method === "paypal" ? "paypal" : "directdebit",
    details: card ? { cardHolder: "Essai Bike360", cardNumber: "4444", cardLabel: "Mastercard", cardFingerprint: "faux", cardExpiryDate: "2029-12-31" }
                  : { consumerName: "Essai Bike360", consumerAccount: "NL55INGB0000000000", consumerBic: "INGBNL2A" },
    mandateReference: null, signatureDate: today(), createdAt: now(), customerId: payment.customerId };
  db.mandates[mandate.id] = mandate;
  return mandate;
}

/** Amène un paiement ouvert à son état final. */
function finish(payment, status, method = "creditcard") {
  if (payment.status !== "open" && payment.status !== "pending") return;
  payment.status = status;
  payment.method = payment.method || method;
  if (status === "paid") {
    payment.paidAt = now();
    payment.amountRefunded = { currency: "EUR", value: "0.00" };
    payment.amountRemaining = payment.amount;
    payment.details = payment.method === "creditcard" ? { cardNumber: "4444", cardLabel: "Mastercard" } : {};
    if (payment.sequenceType === "first") payment.mandateId = mandateFor(payment).id;
  }
}

const usable = (customerId) => Object.values(db.mandates).filter((m) => m.customerId === customerId && (m.status === "valid" || m.status === "pending"));

const api = {
  "POST /v2/customers": (_, body) => {
    const c = { resource: "customer", id: id("cst"), mode: "test", name: body.name ?? null, email: body.email ?? null, locale: body.locale ?? null,
                metadata: body.metadata ?? null, createdAt: now() };
    db.customers[c.id] = c;
    return { status: 201, body: c };
  },
  "DELETE /v2/customers/:customer": ({ customer }) => {
    if (!db.customers[customer]) return missing();
    delete db.customers[customer];
    // « All mandates and subscriptions created for this customer will be canceled as well. »
    for (const s of Object.values(db.subscriptions)) if (s.customerId === customer && s.status === "active") Object.assign(s, { status: "canceled", canceledAt: now(), nextPaymentDate: undefined });
    for (const m of Object.values(db.mandates)) if (m.customerId === customer) m.status = "invalid";
    return { status: 204 };
  },
  "POST /v2/payments": (_, body) => {
    const sequence = body.sequenceType ?? "oneoff";
    if (!money(body.amount)) return invalid("The amount is invalid", "amount");
    if (!body.description) return invalid("The description is required", "description");
    if (!["oneoff", "first", "recurring"].includes(sequence)) return invalid("The sequence type is invalid", "sequenceType");
    if (!body.redirectUrl && sequence !== "recurring") return invalid("The redirect URL is required", "redirectUrl");
    if (body.customerId && !db.customers[body.customerId]) return invalid("The customer does not exist", "customerId");
    if (sequence !== "oneoff" && !body.customerId) return invalid("The customer is required for this sequence type", "customerId");
    // un montant nul n'est admis que pour un premier paiement par carte ou PayPal (guide des paiements récurrents)
    if (Number(body.amount.value) === 0 && !(sequence === "first" && ["creditcard", "paypal"].includes(body.method))) return invalid("The amount is lower than the minimum", "amount");
    const p = { id: id("tr"), createdAt: now(), amount: body.amount, description: body.description, method: body.method ?? null, metadata: body.metadata ?? null,
                status: "open", sequenceType: sequence, customerId: body.customerId, redirectUrl: body.redirectUrl, webhookUrl: body.webhookUrl, locale: body.locale };
    db.payments[p.id] = p;
    return { status: 201, body: view(p) };
  },
  "GET /v2/payments/:payment": ({ payment }) => (db.payments[payment] ? { status: 200, body: view(db.payments[payment]) } : missing()),
  "GET /v2/customers/:customer/mandates": ({ customer }) => {
    if (!db.customers[customer]) return missing();
    const mandates = Object.values(db.mandates).filter((m) => m.customerId === customer).reverse();
    return { status: 200, body: { count: mandates.length, _embedded: { mandates }, _links: { self: link(`${origin}/v2/customers/${customer}/mandates`), previous: null, next: null } } };
  },
  "DELETE /v2/customers/:customer/mandates/:mandate": ({ customer, mandate }) => {
    const m = db.mandates[mandate];
    if (!m || m.customerId !== customer) return missing();
    m.status = "invalid";
    return { status: 204 };
  },
  "POST /v2/customers/:customer/subscriptions": ({ customer }, body) => {
    if (!db.customers[customer]) return missing();
    if (!money(body.amount)) return invalid("The amount is invalid", "amount");
    if (!/^\d+ (days?|weeks?|months?)$/.test(body.interval ?? "")) return invalid("The interval is invalid", "interval");
    const [n, unit] = body.interval.split(" ");
    if (Number(n) > (unit.startsWith("month") ? 12 : unit.startsWith("week") ? 52 : 365)) return invalid("The maximum interval is one year", "interval");
    if (!body.description) return invalid("The description is required", "description");
    if (body.startDate && !/^\d{4}-\d{2}-\d{2}$/.test(body.startDate)) return invalid("The start date is invalid", "startDate");
    // un abonnement demande un mandat « pending » ou « valid » (guide des paiements récurrents) ; le statut du refus est supposé
    const mandates = usable(customer);
    if (!mandates.length) return invalid("The customer has no suitable mandates");
    if (body.mandateId && !mandates.some((m) => m.id === body.mandateId)) return invalid("The mandate is invalid", "mandateId");
    // la description doit être unique parmi les abonnements actifs du client ; le statut du refus est supposé
    if (Object.values(db.subscriptions).some((s) => s.customerId === customer && s.status === "active" && s.description === body.description))
      return invalid("A subscription with this description already exists", "description");
    const start = body.startDate ?? today();
    const s = { resource: "subscription", id: id("sub"), mode: "test", createdAt: now(), status: "active", amount: body.amount, times: body.times ?? null,
                interval: body.interval, startDate: start, nextPaymentDate: start, description: body.description, method: body.method ?? null,
                mandateId: body.mandateId ?? null, webhookUrl: body.webhookUrl, metadata: body.metadata ?? null, customerId: customer };
    db.subscriptions[s.id] = s;
    return { status: 201, body: s };
  },
  "GET /v2/customers/:customer/subscriptions": ({ customer }) => {
    if (!db.customers[customer]) return missing();
    const subscriptions = Object.values(db.subscriptions).filter((s) => s.customerId === customer).reverse();
    return { status: 200, body: { count: subscriptions.length, _embedded: { subscriptions }, _links: { self: link(`${origin}/v2/customers/${customer}/subscriptions`), previous: null, next: null } } };
  },
  "GET /v2/customers/:customer/subscriptions/:subscription": ({ customer, subscription }) => {
    const s = db.subscriptions[subscription];
    return s && s.customerId === customer ? { status: 200, body: s } : missing();
  },
  "PATCH /v2/customers/:customer/subscriptions/:subscription": ({ customer, subscription }, body) => {
    const s = db.subscriptions[subscription];
    if (!s || s.customerId !== customer) return missing();
    // « Canceled subscriptions cannot be updated. » ; le statut du refus est supposé
    if (s.status === "canceled") return invalid("Canceled subscriptions cannot be updated");
    if (body.mandateId && !usable(customer).some((m) => m.id === body.mandateId)) return invalid("The mandate is invalid", "mandateId");
    if (body.amount && !money(body.amount)) return invalid("The amount is invalid", "amount");
    for (const k of ["amount", "description", "interval", "mandateId", "startDate", "times", "metadata", "webhookUrl"]) if (k in body) s[k] = body[k];
    return { status: 200, body: s };
  },
  "DELETE /v2/customers/:customer/subscriptions/:subscription": ({ customer, subscription }) => {
    const s = db.subscriptions[subscription];
    if (!s || s.customerId !== customer) return missing();
    // la documentation ne dit pas ce que répond l'arrêt d'un abonnement déjà arrêté : refus supposé
    if (s.status === "canceled") return invalid("The subscription has been cancelled already");
    Object.assign(s, { status: "canceled", canceledAt: now() });
    delete s.nextPaymentDate;
    return { status: 200, body: s };
  },
};

const control = {
  "POST /_faux/payer/:payment": async ({ payment }, body) => {
    const p = db.payments[payment];
    if (!p) return missing();
    finish(p, body.status ?? "paid", body.method);
    return { status: 200, body: { id: p.id, status: p.status, webhook: await notify(p) } };
  },
  "POST /_faux/echeance/:subscription": async ({ subscription }, body) => {
    const s = db.subscriptions[subscription];
    if (!s || s.status !== "active") return missing();
    const mandate = db.mandates[s.mandateId] ?? usable(s.customerId)[0];
    // « Any metadata added to the subscription will be automatically forwarded to the payments generated for it. »
    const p = { id: id("tr"), createdAt: now(), amount: s.amount, description: s.description, method: mandate?.method ?? "creditcard", metadata: s.metadata,
                status: "open", sequenceType: "recurring", customerId: s.customerId, mandateId: mandate?.id, subscriptionId: s.id, webhookUrl: s.webhookUrl };
    db.payments[p.id] = p;
    finish(p, body.status ?? "paid");
    if (p.status === "paid") s.nextPaymentDate = later(s.nextPaymentDate, s.interval);
    return { status: 200, body: { id: p.id, status: p.status, webhook: await notify(p) } };
  },
  "POST /_faux/reprise/:payment": async ({ payment }, body) => {
    const p = db.payments[payment];
    if (!p || p.status !== "paid") return missing();
    if (body.type === "refund") Object.assign(p, { amountRefunded: p.amount, amountRemaining: { currency: "EUR", value: "0.00" } });
    else p.amountChargedBack = p.amount;
    return { status: 200, body: { id: p.id, webhook: await notify(p) } };
  },
  "POST /_faux/notifier/:payment": async ({ payment }) => {
    const p = db.payments[payment] ?? { id: payment, webhookUrl: Object.values(db.payments)[0]?.webhookUrl };
    return { status: 200, body: { id: p.id, webhook: await notify(p) } };
  },
  "POST /_faux/panne": (_, body) => { outage = Number(body.calls ?? 1); return { status: 200, body: { outage } }; },
  "GET /_faux/etat": () => ({ status: 200, body: db }),
};

/** Route dont le motif (« GET /v2/payments/:payment ») correspond à la requête, avec ses paramètres. */
function match(table, method, path) {
  for (const [pattern, handler] of Object.entries(table)) {
    const [m, p] = pattern.split(" ");
    const names = [];
    const re = new RegExp("^" + p.replace(/:(\w+)/g, (_, n) => { names.push(n); return "([\\w-]+)"; }) + "$");
    const found = m === method && path.match(re);
    if (found) return { handler, params: Object.fromEntries(names.map((n, i) => [n, found[i + 1]])) };
  }
  return null;
}

const page = (p) => `<!doctype html><html lang="fr"><meta charset="utf-8"><title>Faux Mollie</title>
<body style="font:16px system-ui;max-width:520px;margin:60px auto;padding:0 16px">
<h1>Faux Mollie</h1><p>Page de paiement d'essai : rien n'est débité.</p>
<p><strong>${p.description.replace(/[&<>]/g, "")}</strong><br>${p.amount.value.replace(".", ",")} € · ${p.sequenceType}</p>
<form method="post"><button name="issue" value="paid">Payer par carte</button> <button name="issue" value="failed">Carte refusée</button>
<button name="issue" value="canceled">Annuler</button></form></body></html>`;

createServer(async (req, res) => {
  const url = new URL(req.url, origin);
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const raw = Buffer.concat(chunks).toString();
  const send = (status, body, headers = {}) => {
    res.writeHead(status, { "Content-Type": "application/hal+json", ...headers });
    res.end(status === 204 || body === undefined ? "" : typeof body === "string" ? body : JSON.stringify(body));
  };
  // page de paiement : ce que le client voit chez Mollie
  const shown = url.pathname.match(/^\/checkout\/(tr_\w+)$/);
  if (shown) {
    const p = db.payments[shown[1]];
    if (!p) return send(404, "paiement inconnu", { "Content-Type": "text/plain; charset=utf-8" });
    if (req.method === "POST") {
      finish(p, new URLSearchParams(raw).get("issue") || "paid");
      await notify(p);
      return send(303, "", { Location: p.redirectUrl });
    }
    return send(200, page(p), { "Content-Type": "text/html; charset=utf-8" });
  }
  let body = {};
  try { body = raw ? JSON.parse(raw) : {}; } catch { return send(400, fail(400, "Bad Request", "The request body is not valid JSON").body); }
  const piloted = match(control, req.method, url.pathname);
  if (piloted) {
    const out = await piloted.handler(piloted.params, body);
    return send(out.status, out.body);
  }
  db.calls.push({ method: req.method, path: url.pathname, body, idempotency: req.headers["idempotency-key"] ?? null });
  if (!/^Bearer (test|live)_\w+$/.test(req.headers.authorization ?? "")) return send(401, fail(401, "Unauthorized Request", "Missing authentication, or failed to authenticate").body);
  if (outage > 0) { outage--; return send(503, fail(503, "Service Unavailable", "Faux Mollie en panne").body); }
  const found = match(api, req.method, url.pathname);
  if (!found) return send(404, missing().body);
  // idempotence : une création rejouée sous la même clé rend la première réponse ; sous un autre contenu, 400
  const key = req.method === "POST" && req.headers["idempotency-key"];
  const signature = `${url.pathname} ${raw}`;
  if (key && replays.has(key)) {
    const first = replays.get(key);
    if (first.signature !== signature) return send(400, fail(400, "Bad Request", "The idempotency key was already used for a different request").body);
    return send(first.status, first.body, { "Idempotent-Replayed": "true" });
  }
  const out = found.handler(found.params, body);
  if (key) replays.set(key, { signature, ...out });
  send(out.status, out.body);
}).listen(port, "127.0.0.1", () => console.log(`Faux Mollie → ${origin}`));
