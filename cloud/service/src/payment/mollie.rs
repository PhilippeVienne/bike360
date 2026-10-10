//! Paiement par Mollie (API v2, https://docs.mollie.com/reference).
//!
//! Abonnement annuel : un premier paiement (`sequenceType: first`) sur la page de Mollie crée le
//! mandat ; à sa confirmation, le service crée chez Mollie un abonnement de douze mois qui commence
//! un an plus tard. Mollie prélève alors chaque renouvellement et le notifie. Le crédit d'export
//! est un paiement unique.
//!
//! Une notification de Mollie ne porte que l'identifiant du paiement : le service relit ce paiement
//! auprès de Mollie, et n'agit que sur ce que Mollie en dit. Un paiement n'est appliqué qu'une fois.
//!
//! Mollie n'a ni résiliation « à l'échéance » ni annonce de fin d'abonnement. La fiche du compte
//! garde donc l'échéance payée (`echeance`) : résilier arrête l'abonnement chez Mollie et le compte
//! garde son palier jusqu'à cette date ; un renouvellement refusé est représenté par Mollie pendant
//! quelques jours, et le passage des échéances met fin à l'abonnement s'il n'est toujours pas payé
//! (`payment::lapse`).
//!
//! Réglages : BIKE360_MOLLIE_KEY (clé `live_…` ou `test_…`), BIKE360_MOLLIE_API pour un faux serveur.
//!
//! Index : `pk = paiement#<tr_…>`, `sk = commande` garde chaque paiement que le service a ouvert
//! (compte, nature, montant attendu) ; `pk = paiement#<cst_…>`, `sk = client` rattache un client de
//! Mollie à son compte.
//!
//! Route : POST /api/paiement/mollie   notification de Mollie (formulaire, champ `id`)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{Form, Json};
use chrono::{Months, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{cents, payment, record, recorded, reverse, row, safe, Payment, Provider, Receipt, MAX_CREDIT_MIN};
use crate::account::Scope;
use crate::plans::{iso, Account, Plan, FREE};
use crate::{aws, Ctx, Fail};

pub struct Mollie {
    key: String,
    /// Adresse de l'API de Mollie (remplaçable par un faux serveur pour les essais).
    api: String,
    /// Adresse publique du site : Mollie y renvoie le client et y envoie ses notifications.
    site: String,
}

/// Refus de l'API de Mollie : statut HTTP (0 si elle n'a pas répondu) et explication.
#[derive(Debug)]
struct Refusal {
    status: u16,
    detail: String,
}

impl Refusal {
    /// La ressource n'existe pas, ou plus.
    fn gone(&self) -> bool {
        matches!(self.status, 404 | 410)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mollie a répondu {} : {}", self.status, self.detail)
    }
}

impl std::error::Error for Refusal {}

fn unavailable(what: &'static str) -> impl FnOnce(Refusal) -> Fail {
    move |e| {
        eprintln!("paiement, {what} : {e}");
        Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement ne répond pas".into())
    }
}

/// Montant tel que Mollie l'écrit : euros et deux décimales, en texte.
fn amount(cents: u64) -> Value {
    json!({"currency": "EUR", "value": format!("{}.{:02}", cents / 100, cents % 100)})
}

/// Centimes d'un montant de Mollie, s'il est en euros.
fn cents_of(amount: &Value) -> Option<u64> {
    let (euros, hundredths) = amount["value"].as_str().filter(|_| amount["currency"] == "EUR")?.split_once('.')?;
    (hundredths.len() == 2).then_some(euros.parse::<u64>().ok()? * 100 + hundredths.parse::<u64>().ok()?)
}

/// Vrai si le paiement a été repris au vendeur : contesté auprès de la banque, ou remboursé en entier.
fn taken_back(p: &Value) -> bool {
    let paid = cents_of(&p["amount"]).unwrap_or(0);
    cents_of(&p["amountChargedBack"]).is_some_and(|c| c > 0) || (paid > 0 && cents_of(&p["amountRefunded"]) == Some(paid))
}

fn day(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Minuit (UTC) d'une date « AAAA-MM-JJ » de Mollie, au format des dates de la fiche d'un compte.
fn instant(date: &str) -> Option<String> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().map(|d| format!("{}T00:00:00Z", day(d)))
}

fn a_year_from(date: NaiveDate) -> NaiveDate {
    date.checked_add_months(Months::new(12)).unwrap_or(date)
}

/// Ce qu'un mandat dit du moyen de paiement, en clair et sans rien qui permette de s'en servir.
fn describe(mandate: &Value) -> Value {
    let d = &mandate["details"];
    let text = |k: &str| d[k].as_str().unwrap_or_default();
    let label = match mandate["method"].as_str().unwrap_or_default() {
        "creditcard" => {
            let expiry = NaiveDate::parse_from_str(text("cardExpiryDate"), "%Y-%m-%d").map(|e| e.format(", valable jusqu'en %m/%Y").to_string()).unwrap_or_default();
            format!("Carte {} se terminant par {}{expiry}", text("cardLabel"), text("cardNumber")).replace("  ", " ")
        }
        "directdebit" => {
            let iban = text("consumerAccount");
            format!("Prélèvement SEPA sur le compte se terminant par {}", &iban[iban.len().saturating_sub(4)..])
        }
        "paypal" => "PayPal".to_string(),
        other => other.to_string(),
    };
    json!({"kind": mandate["method"], "label": label, "status": mandate["status"]})
}

impl Mollie {
    /// Réglages lus dans l'environnement ; None si Mollie n'est pas configuré.
    pub fn from_env(site: String, var: &dyn Fn(&str) -> Option<String>) -> Result<Option<Mollie>> {
        let Some(key) = var("BIKE360_MOLLIE_KEY") else { return Ok(None) };
        let api = var("BIKE360_MOLLIE_API").unwrap_or_else(|| "https://api.mollie.com".into());
        Ok(Some(Mollie { key, api: api.trim_end_matches('/').to_string(), site }))
    }

    /// Appel à l'API de Mollie (JSON). `once` est une clé d'idempotence : Mollie ne rejoue pas une
    /// création déjà faite sous cette clé dans l'heure.
    async fn call(&self, method: &'static str, path: String, body: Option<Value>, once: Option<String>) -> Result<Value, Refusal> {
        let (url, key) = (format!("{}{path}", self.api), self.key.clone());
        let done = tokio::task::spawn_blocking(move || {
            let mut req = ureq::request(method, &url).set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(20));
            if let Some(once) = &once {
                req = req.set("Idempotency-Key", once);
            }
            let sent = match body {
                Some(b) => req.send_json(b),
                None => req.call(),
            };
            match sent {
                // une suppression répond sans corps
                Ok(res) if res.status() == 204 => Ok(Value::Null),
                Ok(res) => res.into_json::<Value>().map_err(|e| Refusal { status: 0, detail: format!("réponse illisible : {e}") }),
                Err(ureq::Error::Status(status, res)) => {
                    let detail = res.into_json::<Value>().ok().and_then(|v| v["detail"].as_str().map(String::from)).unwrap_or_default();
                    Err(Refusal { status, detail })
                }
                Err(e) => Err(Refusal { status: 0, detail: e.to_string() }),
            }
        }).await;
        done.unwrap_or_else(|e| Err(Refusal { status: 0, detail: e.to_string() }))
    }

    fn webhook_url(&self) -> String {
        format!("{}/api/paiement/mollie", self.site)
    }

    async fn get_payment(&self, id: &str) -> Result<Value, Refusal> {
        self.call("GET", format!("/v2/payments/{id}"), None, None).await
    }

    /// Crée un paiement et renvoie (identifiant, page de paiement où envoyer le client).
    async fn create_payment(&self, customer: &str, cents: u64, description: &str, sequence: &str, method: Option<&str>, metadata: Value) -> Result<(String, String), Refusal> {
        let mut body = json!({
            "amount": amount(cents), "description": description, "locale": "fr_FR",
            "redirectUrl": format!("{}/ui/palier.html?paiement=retour", self.site), "webhookUrl": self.webhook_url(),
            "customerId": customer, "sequenceType": sequence, "metadata": metadata,
        });
        if let Some(method) = method {
            body["method"] = json!(method);
        }
        let p = self.call("POST", "/v2/payments".into(), Some(body), None).await?;
        match (p["id"].as_str().filter(|i| safe(i)), p["_links"]["checkout"]["href"].as_str()) {
            (Some(id), Some(url)) => Ok((id.to_string(), url.to_string())),
            _ => Err(Refusal { status: 0, detail: "paiement créé sans identifiant ou sans page de paiement".into() }),
        }
    }

    /// Abonnement annuel du client sous cette description : celui qui existe déjà, sinon un nouveau.
    /// La description est unique par client, ce qui rend la création rejouable sans double prélèvement.
    async fn subscribe(&self, customer: &str, description: &str, cents: u64, start: NaiveDate, mandate: Option<&str>, metadata: Value, once: &str) -> Result<Value, Refusal> {
        let listed = self.call("GET", format!("/v2/customers/{customer}/subscriptions?limit=250"), None, None).await?;
        let existing = listed["_embedded"]["subscriptions"].as_array().into_iter().flatten()
            .find(|s| s["description"] == description && matches!(s["status"].as_str(), Some("active" | "pending")));
        if let Some(s) = existing {
            return Ok(s.clone());
        }
        let mut body = json!({"amount": amount(cents), "interval": "12 months", "description": description, "startDate": day(start),
                              "webhookUrl": self.webhook_url(), "metadata": metadata});
        if let Some(mandate) = mandate {
            body["mandateId"] = json!(mandate);
        }
        self.call("POST", format!("/v2/customers/{customer}/subscriptions"), Some(body), Some(format!("abonnement-{once}"))).await
    }

    /// Arrête un abonnement ; qu'il n'existe plus ou soit déjà arrêté n'est pas une erreur.
    async fn unsubscribe(&self, customer: &str, subscription: &str) -> Result<(), Refusal> {
        match self.call("DELETE", format!("/v2/customers/{customer}/subscriptions/{subscription}"), None, None).await {
            Ok(_) => Ok(()),
            // Mollie ne dit pas ce qu'il répond pour un abonnement déjà arrêté : tout refus de la demande elle-même en tient lieu
            Err(e) if (400..500).contains(&e.status) && !matches!(e.status, 401 | 403 | 429) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn mandates(&self, customer: &str) -> Result<Vec<Value>, Refusal> {
        let listed = self.call("GET", format!("/v2/customers/{customer}/mandates?limit=250"), None, None).await?;
        Ok(listed["_embedded"]["mandates"].as_array().cloned().unwrap_or_default())
    }

    /// Client de Mollie du compte : celui de sa fiche, sinon un nouveau, rattaché au compte dans l'index.
    async fn customer(&self, c: &Scope, acc: &Account) -> Result<String, Fail> {
        if let Some(known) = ids(acc).0 {
            return Ok(known.to_string());
        }
        let table = c.table.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "le paiement demande l'index des comptes".into()))?;
        let mut body = json!({"metadata": {"client": c.client}});
        // l'adresse sert à Mollie pour ses propres courriels au client ; le paiement se fait aussi sans elle
        if let Some(auth) = &c.auth {
            if let Ok(email) = auth.email_of(&c.client).await {
                body["email"] = json!(email);
            }
        }
        let created = self.call("POST", "/v2/customers".into(), Some(body), None).await.map_err(unavailable("création du client"))?;
        let id = created["id"].as_str().filter(|i| i.starts_with("cst_") && safe(i))
            .ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement n'a pas renvoyé de client".into()))?;
        c.db.put_item().table_name(table).item("pk", AttributeValue::S(format!("paiement#{id}"))).item("sk", AttributeValue::S("client".into()))
            .item("client", AttributeValue::S(c.client.clone())).send().await.map_err(aws("rattachement du client de paiement"))?;
        c.patch(&[("paiement_client", Some(AttributeValue::S(id.into())))]).await.map_err(aws("fiche du compte"))?;
        Ok(id.to_string())
    }

    /// Ouvre un paiement pour le compte et garde ce que le service en attend : la notification ne
    /// sera appliquée que si Mollie annonce ce compte et ce montant.
    async fn open(&self, c: &Scope, acc: &Account, kind: &str, detail: &str, cents: u64, description: &str, sequence: &str, method: Option<&str>) -> Result<String, Fail> {
        let table = c.table.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "le paiement demande l'index des comptes".into()))?;
        let customer = self.customer(c, acc).await?;
        let metadata = json!({"client": c.client, "nature": kind, "detail": detail});
        let (id, url) = self.create_payment(&customer, cents, description, sequence, method, metadata).await.map_err(unavailable("ouverture du paiement"))?;
        let s = |v: &str| AttributeValue::S(v.to_string());
        c.db.put_item().table_name(table).item("pk", s(&format!("paiement#{id}"))).item("sk", s("commande"))
            .item("client", s(&c.client)).item("nature", s(kind)).item("detail", s(detail))
            .item("centimes", AttributeValue::N(cents.to_string())).item("date", s(&iso(Utc::now())))
            .send().await.map_err(aws("enregistrement de la commande"))?;
        Ok(url)
    }
}

/// Identifiants de Mollie d'une fiche : (client, abonnement). Ceux d'un autre prestataire sont ignorés.
fn ids(acc: &Account) -> (Option<&str>, Option<&str>) {
    (acc.customer.as_deref().filter(|c| c.starts_with("cst_") && safe(c)), acc.subscription.as_deref().filter(|s| s.starts_with("sub_") && safe(s)))
}

fn plan_label(plan: &Plan) -> String {
    format!("Bike360 Cloud, palier {}, un an", plan.label)
}

impl Provider for Mollie {
    fn name(&self) -> &'static str {
        "mollie"
    }

    async fn order(&self, c: &Scope, acc: &Account, plan: &Plan, recovery_eur: Option<f64>) -> Result<String, Fail> {
        let total = cents(plan.eur_year) + recovery_eur.map_or(0, cents);
        let mut description = plan_label(plan);
        if recovery_eur.is_some() {
            description += ", et récupération des rushs archivés";
        }
        self.open(c, acc, "palier", &plan.key, total, &description, "first", None).await
    }

    async fn credit(&self, c: &Scope, acc: &Account, minutes: u32) -> Result<String, Fail> {
        let total = cents(c.policy.credit_eur * minutes as f64);
        self.open(c, acc, "credit", &minutes.to_string(), total, &format!("Bike360 Cloud, crédit d'export de {minutes} minutes"), "oneoff", None).await
    }

    async fn cancel(&self, c: &Scope, acc: &Account, undo: bool) -> Result<Option<String>, Fail> {
        let (Some(customer), Some(subscription)) = ids(acc) else { return Err(Fail(StatusCode::CONFLICT, "ce compte n'a pas d'abonnement en cours".into())) };
        if !undo {
            self.unsubscribe(customer, subscription).await.map_err(unavailable("résiliation"))?;
            return Ok(Some(acc.paid_until.clone().unwrap_or_else(|| iso(Utc::now()))));
        }
        // revenir sur la résiliation : l'abonnement arrêté ne repart pas, un autre reprend à l'échéance déjà payée
        let until = acc.paid_until.as_deref().and_then(|t| NaiveDate::parse_from_str(&t[..t.len().min(10)], "%Y-%m-%d").ok())
            .filter(|d| *d > Utc::now().date_naive() && acc.cancel_at.is_some())
            .ok_or_else(|| Fail(StatusCode::CONFLICT, "cet abonnement est arrivé à son terme : reprends un palier".into()))?;
        let usable = self.mandates(customer).await.map_err(unavailable("lecture des mandats"))?.iter()
            .any(|m| matches!(m["status"].as_str(), Some("valid" | "pending")));
        if !usable {
            return Err(Fail(StatusCode::CONFLICT, "ton moyen de paiement n'est plus utilisable : reprends un palier pour en enregistrer un".into()));
        }
        let plan = c.plan_of(acc);
        let description = format!("{} (reprise du {})", plan_label(plan), day(Utc::now().date_naive()));
        let sub = self.subscribe(customer, &description, cents(plan.eur_year), until, None, json!({"client": c.client, "palier": plan.key}),
                                 &format!("{customer}-{}", day(until))).await.map_err(unavailable("reprise de l'abonnement"))?;
        let id = sub["id"].as_str().filter(|i| safe(i)).ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement n'a pas renvoyé d'abonnement".into()))?;
        c.patch(&[("paiement_abonnement", Some(AttributeValue::S(id.into())))]).await.map_err(aws("fiche du compte"))?;
        Ok(None)
    }

    /// Supprimer le client chez Mollie arrête aussi ses mandats et ses abonnements.
    async fn end_now(&self, acc: &Account) -> Result<(), Fail> {
        let Some(customer) = ids(acc).0 else { return Ok(()) };
        match self.call("DELETE", format!("/v2/customers/{customer}"), None, None).await {
            Err(e) if !e.gone() => Err(unavailable("suppression du client")(e)),
            _ => Ok(()),
        }
    }

    async fn method(&self, acc: &Account) -> Result<Option<Value>, Fail> {
        let Some(customer) = ids(acc).0 else { return Ok(None) };
        let mandates = self.mandates(customer).await.map_err(unavailable("lecture des mandats"))?;
        let usable = |m: &&Value| matches!(m["status"].as_str(), Some("valid" | "pending"));
        // celui de l'abonnement s'il sert encore, sinon le plus récent qui serve
        let of_subscription = mandates.iter().filter(usable).find(|m| acc.mandate.is_some() && m["id"].as_str() == acc.mandate.as_deref());
        Ok(of_subscription.or_else(|| mandates.iter().find(usable)).map(describe))
    }

    /// Un premier paiement de 0 € par carte enregistre un nouveau mandat sans rien débiter.
    async fn change_method(&self, c: &Scope, acc: &Account) -> Result<String, Fail> {
        if ids(acc).1.is_none() || acc.cancel_at.is_some() {
            return Err(Fail(StatusCode::CONFLICT, "ce compte n'a pas d'abonnement en cours".into()));
        }
        self.open(c, acc, "moyen", "", 0, "Bike360 Cloud, nouveau moyen de paiement", "first", Some("creditcard")).await
    }
}

impl Mollie {
    /// Applique ce que Mollie dit d'un paiement. Rejouable : chaque effet n'a lieu qu'une fois.
    async fn settle(&self, ctx: &Arc<Ctx>, p: &Value) -> Result<&'static str> {
        let id = p["id"].as_str().filter(|i| safe(i)).context("paiement sans identifiant")?;
        let table = ctx.table.as_ref().context("index non configuré")?;
        if let Some(subscription) = p["subscriptionId"].as_str() {
            return self.renewal(ctx, table, p, id, subscription).await;
        }
        if p["status"] != "paid" {
            return Ok("ignorée : paiement non abouti");
        }
        // seul un paiement ouvert par ce service, pour ce compte et ce montant, change un compte
        let Some(order) = row(ctx, format!("paiement#{id}"), "commande").await.map_err(|f| anyhow::anyhow!(f.1))? else {
            return Ok("ignorée : paiement que ce service n'a pas ouvert");
        };
        let text = |k: &str| order.get(k).and_then(|v| v.as_s().ok()).cloned().unwrap_or_default();
        let (client, kind, detail) = (text("client"), text("nature"), text("detail"));
        let expected = order.get("centimes").and_then(|v| v.as_n().ok()).and_then(|n| n.parse::<u64>().ok());
        let paid = cents_of(&p["amount"]);
        if paid.is_none() || paid != expected || p["metadata"]["client"] != client.as_str() {
            eprintln!("paiement {id} : montant ou compte inattendu, rien n'est appliqué");
            return Ok("ignorée : montant ou compte inattendu");
        }
        let (paid, scope) = (paid.unwrap_or(0), Scope::of(ctx.clone(), client.clone()));
        if taken_back(p) {
            return self.took_back(&scope, id).await;
        }
        let method = p["method"].as_str();
        match kind.as_str() {
            "credit" => {
                let Some(minutes) = detail.parse::<u32>().ok().filter(|m| *m > 0 && *m <= MAX_CREDIT_MIN) else { return Ok("ignorée : crédit hors bornes") };
                let receipt = Receipt { id, client: &client, kind: "credit", label: format!("Crédit d'export, {minutes} minutes"), cents: paid, method,
                                        credit_s: minutes as f64 * 60.0 };
                Ok(if record(ctx, &receipt).await? { "crédit ajouté" } else { "ignorée : crédit déjà ajouté" })
            }
            "palier" => {
                if recorded(ctx, table, id).await?.is_some() {
                    return Ok("ignorée : abonnement déjà pris");
                }
                let Some(plan) = ctx.plans.iter().find(|x| x.key == detail && x.key != FREE) else { return Ok("ignorée : palier inconnu") };
                let Some(customer) = p["customerId"].as_str().filter(|c| c.starts_with("cst_") && safe(c)) else { return Ok("ignorée : paiement sans client") };
                let acc = scope.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
                // ce paiement couvre la première année : l'abonnement prélèvera la suivante
                let start = a_year_from(Utc::now().date_naive());
                let sub = self.subscribe(customer, &format!("{} ({id})", plan_label(plan)), cents(plan.eur_year), start, p["mandateId"].as_str(),
                                         json!({"client": client, "palier": plan.key}), id).await?;
                let sub_id = sub["id"].as_str().filter(|i| safe(i)).context("abonnement sans identifiant")?;
                // changement de palier : l'abonnement précédent ne doit plus rien prélever
                if let (Some(old_customer), Some(old)) = ids(&acc) {
                    if old != sub_id {
                        self.unsubscribe(old_customer, old).await?;
                    }
                }
                let until = sub["startDate"].as_str().and_then(instant).unwrap_or_else(|| format!("{}T00:00:00Z", day(start)));
                scope.subscribed(&plan.key, Some(customer), Some(sub_id), Some(&until)).await?;
                scope.patch(&[("paiement_mandat", p["mandateId"].as_str().map(|m| AttributeValue::S(m.into())))]).await?;
                let extra = paid.saturating_sub(cents(plan.eur_year));
                let label = if extra > 0 { format!("Palier {}, un an, et récupération des rushs archivés", plan.label) } else { format!("Palier {}, un an", plan.label) };
                record(ctx, &Receipt { id, client: &client, kind: "abonnement", label, cents: paid, method, credit_s: 0.0 }).await?;
                Ok("palier mis à jour")
            }
            "moyen" => {
                let acc = scope.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
                let (Some(customer), Some(subscription), Some(mandate)) = (ids(&acc).0, ids(&acc).1, p["mandateId"].as_str()) else {
                    return Ok("ignorée : pas d'abonnement à rattacher à ce moyen de paiement");
                };
                if acc.mandate.as_deref() == Some(mandate) {
                    return Ok("ignorée : moyen de paiement déjà rattaché");
                }
                self.call("PATCH", format!("/v2/customers/{customer}/subscriptions/{subscription}"), Some(json!({"mandateId": mandate})), None).await?;
                scope.patch(&[("paiement_mandat", Some(AttributeValue::S(mandate.into())))]).await?;
                Ok("moyen de paiement changé")
            }
            _ => Ok("ignorée : nature inconnue"),
        }
    }

    /// Paiement créé par Mollie pour un abonnement : renouvellement payé, ou refusé.
    async fn renewal(&self, ctx: &Arc<Ctx>, table: &str, p: &Value, id: &str, subscription: &str) -> Result<&'static str> {
        let Some(customer) = p["customerId"].as_str().filter(|c| c.starts_with("cst_") && safe(c)) else { return Ok("ignorée : renouvellement sans client") };
        let Some(found) = row(ctx, format!("paiement#{customer}"), "client").await.map_err(|f| anyhow::anyhow!(f.1))? else { return Ok("ignorée : client inconnu") };
        let Some(client) = found.get("client").and_then(|v| v.as_s().ok()).cloned() else { return Ok("ignorée : client inconnu") };
        let scope = Scope::of(ctx.clone(), client.clone());
        let acc = scope.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        let current = ids(&acc).1 == Some(subscription);
        match p["status"].as_str().unwrap_or_default() {
            "paid" if taken_back(p) => self.took_back(&scope, id).await,
            "paid" => {
                if recorded(ctx, table, id).await?.is_some() {
                    return Ok("ignorée : renouvellement déjà compté");
                }
                if !current {
                    eprintln!("paiement {id} : encaissé pour l'abonnement {subscription}, qui n'est plus celui du compte {client} ; à rembourser à la main");
                    return Ok("ignorée : abonnement qui n'est plus celui du compte");
                }
                // l'échéance suivante est celle que Mollie annonce
                let sub = self.call("GET", format!("/v2/customers/{customer}/subscriptions/{subscription}"), None, None).await?;
                let until = sub["nextPaymentDate"].as_str().and_then(instant)
                    .unwrap_or_else(|| format!("{}T00:00:00Z", day(a_year_from(Utc::now().date_naive()))));
                scope.patch(&[("echeance", Some(AttributeValue::S(until))), ("paiement_echec", None)]).await?;
                let label = format!("Palier {}, un an (renouvellement)", scope.plan_of(&acc).label);
                record(ctx, &Receipt { id, client: &client, kind: "renouvellement", label, cents: cents_of(&p["amount"]).unwrap_or(0),
                                       method: p["method"].as_str(), credit_s: 0.0 }).await?;
                Ok("abonnement renouvelé")
            }
            "failed" | "expired" | "canceled" if current && acc.failed.is_none() => {
                scope.patch(&[("paiement_echec", Some(AttributeValue::S(iso(Utc::now()))))]).await?;
                let sent = scope.mail("Bike360 : le renouvellement de ton abonnement n'a pas pu être prélevé", &format!(
                    "Bonjour,\n\nTa banque a refusé le prélèvement du renouvellement de ton abonnement. Il sera représenté dans les prochains jours. \
                     Sans paiement d'ici {} jours, l'abonnement s'arrêtera. Tu peux régler tout de suite depuis ta page de palier :\n\n{}/ui/palier.html\n",
                    ctx.policy.grace_days, ctx.site)).await;
                if let Err(e) = sent {
                    eprintln!("courriel d'échec de paiement à {client} : {e:#}");
                }
                Ok("échec du renouvellement noté")
            }
            _ => Ok("ignorée : renouvellement sans suite"),
        }
    }

    /// Paiement repris au vendeur (contestation, remboursement complet) : ce qu'il avait apporté est retiré.
    async fn took_back(&self, scope: &Scope, id: &str) -> Result<&'static str> {
        let Some((_, kind, seconds)) = reverse(scope, id).await? else { return Ok("ignorée : paiement repris, sans effet à retirer") };
        let acc = scope.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        if kind == "credit" {
            scope.patch(&[("credit_s", Some(AttributeValue::N(format!("{:.1}", (acc.credit_s - seconds).max(0.0)))))]).await?;
            return Ok("paiement repris : crédit retiré");
        }
        if let (Some(customer), Some(subscription)) = ids(&acc) {
            self.unsubscribe(customer, subscription).await?;
        }
        if scope.plan_of(&acc).key != FREE {
            scope.unsubscribed().await?;
        }
        println!("paiement {id} repris : abonnement de {} terminé", scope.client);
        Ok("paiement repris : abonnement terminé")
    }
}

#[derive(Deserialize)]
pub struct Notice {
    id: String,
}

/// Notification de Mollie : l'identifiant d'un paiement qui a changé, et rien d'autre. Le paiement est
/// relu auprès de Mollie ; la réponse ne dit jamais à l'appelant ce qui en a été fait.
pub async fn webhook(State(ctx): State<Arc<Ctx>>, Form(n): Form<Notice>) -> Result<Json<Value>, Fail> {
    let Payment::Mollie(m) = payment(&ctx)? else { return Err(Fail(StatusCode::NOT_FOUND, "ce prestataire n'est pas celui du service".into())) };
    let quiet = Ok(Json(json!({"ok": true})));
    if !n.id.starts_with("tr_") || !safe(&n.id) {
        return quiet;
    }
    let p = match m.get_payment(&n.id).await {
        Ok(p) => p,
        // identifiant inconnu de Mollie : même réponse que pour un vrai, pour ne rien apprendre à qui en essaie
        Err(e) if e.gone() => return quiet,
        Err(e) => return Err(unavailable("lecture du paiement")(e)),
    };
    if p["id"] != n.id.as_str() {
        return quiet;
    }
    match m.settle(&ctx, &p).await {
        Ok(done) => {
            println!("paiement {} ({}) : {done}", n.id, p["status"].as_str().unwrap_or("?"));
            quiet
        }
        // une erreur de notre côté : Mollie représentera la notification
        Err(e) => {
            eprintln!("notification du paiement {} : {e:#}", n.id);
            Err(Fail(StatusCode::INTERNAL_SERVER_ERROR, "notification non appliquée".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;

    /// Faux Mollie minimal : sert les réponses données, dans l'ordre, et garde ce qu'il a reçu
    /// (ligne de requête, en-têtes en minuscules, corps). Rien ne sort de la machine.
    fn fake(replies: Vec<(u16, Value)>) -> (Mollie, Arc<Mutex<Vec<(String, String, Value)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, reply) in replies {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream);
                let (mut first, mut headers, mut length) = (String::new(), String::new(), 0usize);
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    let line = line.to_ascii_lowercase();
                    if let Some(n) = line.strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }
                    headers += &line;
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push((first.trim().to_string(), headers, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let text = if status == 204 { String::new() } else { reply.to_string() };
                write!(reader.get_mut(), "HTTP/1.1 {status} X\r\nContent-Type: application/hal+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
            }
        });
        (Mollie { key: "test_essai".into(), api, site: "https://bike360.exemple".into() }, seen)
    }

    #[test]
    fn amounts_are_written_and_read_as_mollie_does() {
        assert_eq!(amount(3900), json!({"currency": "EUR", "value": "39.00"}));
        assert_eq!(amount(305), json!({"currency": "EUR", "value": "3.05"}));
        assert_eq!(amount(0)["value"], "0.00");
        assert_eq!(cents_of(&json!({"currency": "EUR", "value": "105.00"})), Some(10500));
        for bad in [json!({"currency": "USD", "value": "39.00"}), json!({"currency": "EUR", "value": "39"}), json!({"currency": "EUR", "value": "39.0"}),
                    json!({"currency": "EUR", "value": 39.0}), json!(null)] {
            assert_eq!(cents_of(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_payment_is_taken_back_by_a_chargeback_or_a_full_refund() {
        let paid = |extra: Value| {
            let mut p = json!({"amount": {"currency": "EUR", "value": "39.00"}, "status": "paid"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            p
        };
        assert!(!taken_back(&paid(json!({}))));
        assert!(!taken_back(&paid(json!({"amountRefunded": {"currency": "EUR", "value": "0.00"}}))));
        assert!(!taken_back(&paid(json!({"amountRefunded": {"currency": "EUR", "value": "10.00"}}))), "remboursement partiel");
        assert!(taken_back(&paid(json!({"amountRefunded": {"currency": "EUR", "value": "39.00"}}))));
        assert!(taken_back(&paid(json!({"amountChargedBack": {"currency": "EUR", "value": "39.00"}}))));
    }

    #[test]
    fn a_mandate_is_described_without_what_would_let_someone_use_it() {
        let card = describe(&json!({"method": "creditcard", "status": "valid",
            "details": {"cardHolder": "A B", "cardNumber": "4444", "cardLabel": "Mastercard", "cardExpiryDate": "2028-03-31", "cardFingerprint": "x"}}));
        assert_eq!(card["label"], "Carte Mastercard se terminant par 4444, valable jusqu'en 03/2028");
        let debit = describe(&json!({"method": "directdebit", "status": "pending", "details": {"consumerName": "A B", "consumerAccount": "FR7630006000011234567890189"}}));
        assert_eq!(debit["label"], "Prélèvement SEPA sur le compte se terminant par 0189");
        assert_eq!(debit["status"], "pending");
    }

    #[test]
    fn renewal_dates_follow_the_calendar() {
        let d = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(day(a_year_from(d("2026-10-10"))), "2027-10-10");
        assert_eq!(day(a_year_from(d("2028-02-29"))), "2029-02-28");
        assert_eq!(instant("2027-10-10").as_deref(), Some("2027-10-10T00:00:00Z"));
        assert_eq!(instant("10/10/2027"), None);
    }

    #[tokio::test]
    async fn a_first_payment_is_created_as_the_reference_describes() {
        let (m, seen) = fake(vec![(201, json!({"id": "tr_un", "status": "open", "_links": {"checkout": {"href": "https://payer.exemple/tr_un"}}}))]);
        let got = m.create_payment("cst_a", 3900, "Bike360 Cloud, palier 600 Go, un an", "first", None, json!({"client": "c1"})).await.unwrap();
        assert_eq!(got, ("tr_un".to_string(), "https://payer.exemple/tr_un".to_string()));
        let (first, headers, body) = seen.lock().unwrap()[0].clone();
        assert_eq!(first, "POST /v2/payments HTTP/1.1");
        assert!(headers.contains("authorization: bearer test_essai"), "{headers}");
        assert_eq!(body, json!({
            "amount": {"currency": "EUR", "value": "39.00"}, "description": "Bike360 Cloud, palier 600 Go, un an", "locale": "fr_FR",
            "redirectUrl": "https://bike360.exemple/ui/palier.html?paiement=retour", "webhookUrl": "https://bike360.exemple/api/paiement/mollie",
            "customerId": "cst_a", "sequenceType": "first", "metadata": {"client": "c1"},
        }));
    }

    #[tokio::test]
    async fn a_subscription_is_created_once_per_description() {
        let start = NaiveDate::from_ymd_opt(2027, 10, 10).unwrap();
        let (m, seen) = fake(vec![
            (200, json!({"count": 0, "_embedded": {"subscriptions": []}})),
            (201, json!({"id": "sub_un", "status": "active", "startDate": "2027-10-10"})),
            (200, json!({"count": 2, "_embedded": {"subscriptions": [
                {"id": "sub_vieux", "status": "canceled", "description": "palier (tr_un)"}, {"id": "sub_un", "status": "active", "description": "palier (tr_un)"}]}})),
        ]);
        let created = m.subscribe("cst_a", "palier (tr_un)", 3900, start, Some("mdt_1"), json!({"client": "c1"}), "tr_un").await.unwrap();
        assert_eq!(created["id"], "sub_un");
        let again = m.subscribe("cst_a", "palier (tr_un)", 3900, start, Some("mdt_1"), json!({"client": "c1"}), "tr_un").await.unwrap();
        assert_eq!(again["id"], "sub_un", "l'abonnement déjà actif est repris, pas recréé");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "une seule création pour deux demandes");
        assert_eq!(seen[0].0, "GET /v2/customers/cst_a/subscriptions?limit=250 HTTP/1.1");
        assert_eq!(seen[1].0, "POST /v2/customers/cst_a/subscriptions HTTP/1.1");
        assert!(seen[1].1.contains("idempotency-key: abonnement-tr_un"), "{}", seen[1].1);
        assert_eq!(seen[1].2, json!({"amount": {"currency": "EUR", "value": "39.00"}, "interval": "12 months", "description": "palier (tr_un)",
            "startDate": "2027-10-10", "webhookUrl": "https://bike360.exemple/api/paiement/mollie", "metadata": {"client": "c1"}, "mandateId": "mdt_1"}));
    }

    #[tokio::test]
    async fn refusals_keep_their_status_and_a_missing_subscription_is_already_stopped() {
        let (m, seen) = fake(vec![
            (404, json!({"status": 404, "title": "Not Found", "detail": "No entity with this ID exists."})),
            (204, Value::Null),
            (404, json!({"status": 404, "title": "Not Found", "detail": "No payment exists with token tr_x."})),
            (500, json!({"status": 500, "title": "Internal Server Error", "detail": "panne"})),
        ]);
        assert!(m.unsubscribe("cst_a", "sub_parti").await.is_ok());
        assert!(m.unsubscribe("cst_a", "sub_un").await.is_ok());
        assert_eq!(seen.lock().unwrap()[1].0, "DELETE /v2/customers/cst_a/subscriptions/sub_un HTTP/1.1");
        let missing = m.get_payment("tr_x").await.unwrap_err();
        assert!(missing.gone() && missing.detail.contains("tr_x"));
        let down = m.get_payment("tr_y").await.unwrap_err();
        assert!(!down.gone() && down.status == 500);
    }
}
