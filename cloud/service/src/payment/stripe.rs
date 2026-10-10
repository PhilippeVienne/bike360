//! Paiement par Stripe, gardé derrière la même interface que Mollie mais plus utilisé par le service
//! (`BIKE360_PAYMENT=stripe` pour y revenir) : page de paiement de Stripe, puis notification signée.
//! Stripe a son propre portail client : les écrans de moyen de paiement ne sont pas branchés ici.
//!
//! Réglages : BIKE360_STRIPE_KEY, BIKE360_STRIPE_WEBHOOK_SECRET, BIKE360_STRIPE_PRICES
//! (« 600go=price_…,1to=price_… »), BIKE360_STRIPE_API pour un émulateur.
//!
//! Route : POST /api/paiement/stripe   notification de Stripe (corps brut, en-tête Stripe-Signature)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use super::{cents, payment, record, safe, Payment, Provider, Receipt, MAX_CREDIT_MIN};
use crate::account::Scope;
use crate::plans::{Account, Plan, FREE};
use crate::{Ctx, Fail};

/// Écart toléré entre l'heure d'une notification et la nôtre (contre le rejeu).
const TOLERANCE_S: u64 = 300;

pub struct Stripe {
    secret_key: String,
    webhook_secret: String,
    /// Adresse de l'API de Stripe (remplaçable par un émulateur pour les essais).
    api: String,
    /// Palier → identifiant de prix chez Stripe.
    prices: HashMap<String, String>,
    /// Adresse publique du site, où Stripe renvoie le client après le paiement.
    site: String,
}

impl Stripe {
    /// Réglages lus dans l'environnement ; None si Stripe n'est pas configuré.
    pub fn from_env(site: String, var: &dyn Fn(&str) -> Option<String>) -> Result<Option<Stripe>> {
        let (Some(secret_key), Some(webhook_secret)) = (var("BIKE360_STRIPE_KEY"), var("BIKE360_STRIPE_WEBHOOK_SECRET")) else { return Ok(None) };
        let prices: HashMap<String, String> = var("BIKE360_STRIPE_PRICES").unwrap_or_default().split(',')
            .filter_map(|p| p.trim().split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        anyhow::ensure!(!prices.is_empty(), "BIKE360_STRIPE_PRICES est vide : aucun palier n'est vendable");
        let api = var("BIKE360_STRIPE_API").unwrap_or_else(|| "https://api.stripe.com".into());
        Ok(Some(Stripe { secret_key, webhook_secret, api: api.trim_end_matches('/').to_string(), prices, site }))
    }

    /// La signature couvre « horodatage.corps » ; elle doit être récente et correspondre à notre secret.
    fn verify(&self, header: &str, body: &[u8], now: u64) -> bool {
        let field = |name: &'static str| header.split(',').filter_map(|p| p.trim().split_once('=')).filter(move |(k, _)| *k == name).map(|(_, v)| v);
        let Some(t) = field("t").next().and_then(|t| t.parse::<u64>().ok()) else { return false };
        if now.abs_diff(t) > TOLERANCE_S {
            return false;
        }
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(self.webhook_secret.as_bytes()) else { return false };
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        let expected: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        // comparaison en temps constant
        field("v1").any(|sig| sig.len() == expected.len() && sig.bytes().zip(expected.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0)
    }

    /// Appel à l'API de Stripe (formulaire) ; une erreur n'est pas détaillée au navigateur.
    async fn call(&self, method: &'static str, path: String, form: Vec<(String, String)>) -> Result<Value, Fail> {
        let (url, key) = (format!("{}{path}", self.api), self.secret_key.clone());
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let req = ureq::request(method, &url).set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(20));
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            Ok(if form.is_empty() { req.call()? } else { req.send_form(&form)? }.into_json()?)
        }).await.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r).map_err(|e| {
            eprintln!("paiement, {method} {path} : {e:#}");
            Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement ne répond pas".into())
        })
    }

    /// Page de paiement pour ces lignes, rattachée au compte.
    async fn checkout(&self, client: &str, mode: &str, mut form: Vec<(String, String)>) -> Result<String, Fail> {
        form.extend([("mode", mode.to_string()), ("client_reference_id", client.to_string()),
                     ("success_url", format!("{}/ui/palier.html?paiement=ok", self.site)), ("cancel_url", format!("{}/ui/palier.html", self.site))]
            .map(|(k, v)| (k.to_string(), v)));
        let session = self.call("POST", "/v1/checkout/sessions".into(), form).await?;
        session["url"].as_str().map(String::from).ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement n'a pas renvoyé de page".into()))
    }
}

/// Ligne de paiement ponctuel à prix libre : `cents` par unité.
fn line(n: usize, name: &str, cents: u64, quantity: u64) -> Vec<(String, String)> {
    [("price_data][currency", "eur".to_string()), ("price_data][unit_amount", cents.to_string()),
     ("price_data][product_data][name", name.to_string()), ("quantity", quantity.to_string())]
        .map(|(k, v)| (format!("line_items[{n}][{k}]"), v)).to_vec()
}

impl Provider for Stripe {
    fn name(&self) -> &'static str {
        "stripe"
    }

    async fn order(&self, c: &Scope, _acc: &Account, plan: &Plan, recovery_eur: Option<f64>) -> Result<String, Fail> {
        let price = self.prices.get(&plan.key).ok_or_else(|| Fail(StatusCode::BAD_REQUEST, "ce palier n'est pas en vente".into()))?;
        let mut form = vec![("line_items[0][price]".to_string(), price.clone()), ("line_items[0][quantity]".into(), "1".into()),
                            ("metadata[palier]".into(), plan.key.clone())];
        if let Some(eur) = recovery_eur {
            form.extend(line(1, "Récupération des rushs archivés", cents(eur), 1));
        }
        self.checkout(&c.client, "subscription", form).await
    }

    async fn credit(&self, c: &Scope, _acc: &Account, minutes: u32) -> Result<String, Fail> {
        let mut form = line(0, "Minute d'export", cents(c.policy.credit_eur), minutes as u64);
        form.push(("metadata[credit_min]".into(), minutes.to_string()));
        self.checkout(&c.client, "payment", form).await
    }

    async fn cancel(&self, _c: &Scope, acc: &Account, undo: bool) -> Result<Option<String>, Fail> {
        let sub = acc.subscription.as_deref().filter(|s| safe(s)).ok_or_else(|| Fail(StatusCode::CONFLICT, "ce compte n'a pas d'abonnement en cours".into()))?;
        let got = self.call("POST", format!("/v1/subscriptions/{sub}"), vec![("cancel_at_period_end".into(), (!undo).to_string())]).await?;
        // sans date annoncée, la fin viendra par la notification de Stripe
        Ok(got["cancel_at"].as_i64().or_else(|| got["current_period_end"].as_i64())
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)).filter(|_| !undo).map(crate::plans::iso))
    }

    async fn end_now(&self, acc: &Account) -> Result<(), Fail> {
        let Some(sub) = acc.subscription.as_deref().filter(|s| safe(s)) else { return Ok(()) };
        self.call("DELETE", format!("/v1/subscriptions/{sub}"), vec![]).await.map(|_| ())
    }

    async fn method(&self, _acc: &Account) -> Result<Option<Value>, Fail> {
        Ok(None)
    }

    async fn change_method(&self, _c: &Scope, _acc: &Account) -> Result<String, Fail> {
        Err(Fail(StatusCode::NOT_FOUND, "le changement de moyen de paiement n'est pas proposé avec ce prestataire".into()))
    }
}

/// Applique une notification déjà authentifiée : abonnement payé → palier ; crédit payé → minutes
/// d'export ; abonnement terminé → essai.
async fn apply(ctx: &Arc<Ctx>, event: &Value) -> Result<&'static str> {
    let table = ctx.table.as_ref().context("index non configuré")?;
    let obj = &event["data"]["object"];
    match event["type"].as_str().unwrap_or_default() {
        "checkout.session.completed" => {
            let Some(client) = obj["client_reference_id"].as_str().filter(|c| safe(c)) else { return Ok("ignorée : commande sans compte") };
            if !matches!(obj["payment_status"].as_str(), Some("paid" | "no_payment_required")) {
                return Ok("ignorée : paiement non abouti");
            }
            if let Some(minutes) = obj["metadata"]["credit_min"].as_str().and_then(|m| m.parse::<u32>().ok()) {
                let Some(id) = obj["id"].as_str().filter(|i| safe(i)) else { return Ok("ignorée : paiement sans identifiant") };
                if minutes == 0 || minutes > MAX_CREDIT_MIN {
                    return Ok("ignorée : crédit hors bornes");
                }
                let receipt = Receipt { id, client, kind: "credit", label: format!("Crédit d'export, {minutes} minutes"),
                                        cents: obj["amount_total"].as_u64().unwrap_or(0), method: None, credit_s: minutes as f64 * 60.0 };
                return Ok(if record(ctx, &receipt).await? { "crédit ajouté" } else { "ignorée : crédit déjà ajouté" });
            }
            let Some(plan) = obj["metadata"]["palier"].as_str() else { return Ok("ignorée : commande sans palier") };
            if !ctx.plans.iter().any(|p| p.key == plan && p.key != FREE) {
                return Ok("ignorée : palier inconnu");
            }
            let customer = obj["customer"].as_str();
            Scope::of(ctx.clone(), client.to_string()).subscribed(plan, customer, obj["subscription"].as_str(), None).await?;
            if let Some(customer) = customer.filter(|c| safe(c)) {
                // pour retrouver le compte quand Stripe annonce la fin de l'abonnement
                ctx.db.put_item().table_name(table).item("pk", AttributeValue::S(format!("paiement#{customer}")))
                    .item("sk", AttributeValue::S("client".into())).item("client", AttributeValue::S(client.into())).send().await?;
            }
            Ok("palier mis à jour")
        }
        "customer.subscription.deleted" => {
            let Some(customer) = obj["customer"].as_str().filter(|c| safe(c)) else { return Ok("ignorée : abonnement sans client") };
            let found = ctx.db.get_item().table_name(table).key("pk", AttributeValue::S(format!("paiement#{customer}")))
                .key("sk", AttributeValue::S("client".into())).send().await?;
            let Some(client) = found.item().and_then(|i| i.get("client")).and_then(|v| v.as_s().ok()).cloned() else { return Ok("ignorée : client inconnu") };
            let scope = Scope::of(ctx.clone(), client);
            // un compte déjà revenu à l'essai garde la date de fin qu'il a : le délai d'accès ne repart pas
            if scope.plan().await.map_err(|f| anyhow::anyhow!(f.1))?.key == FREE {
                return Ok("ignorée : abonnement déjà terminé");
            }
            scope.unsubscribed().await?;
            Ok("retour au palier d'essai")
        }
        _ => Ok("ignorée : type non traité"),
    }
}

/// Notification de Stripe. Sans signature valable, rien n'est lu ni appliqué.
pub async fn webhook(State(ctx): State<Arc<Ctx>>, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, Fail> {
    let Payment::Stripe(p) = payment(&ctx)? else { return Err(Fail(StatusCode::NOT_FOUND, "ce prestataire n'est pas celui du service".into())) };
    let sig = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if !p.verify(sig, &body, now) {
        return Err(Fail(StatusCode::BAD_REQUEST, "signature invalide".into()));
    }
    let event: Value = serde_json::from_slice(&body).map_err(|_| Fail(StatusCode::BAD_REQUEST, "notification illisible".into()))?;
    match apply(&ctx, &event).await {
        Ok(done) => Ok(Json(json!({"ok": true, "result": done}))),
        // une erreur de notre côté : Stripe représentera la notification
        Err(e) => {
            eprintln!("notification de paiement : {e:#}");
            Err(Fail(StatusCode::INTERNAL_SERVER_ERROR, "notification non appliquée".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment() -> Stripe {
        Stripe { secret_key: "sk".into(), webhook_secret: "whsec_essai".into(), api: String::new(), prices: HashMap::new(), site: String::new() }
    }

    fn sign(secret: &str, t: u64, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{t}.{body}").as_bytes());
        let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        format!("t={t},v1={hex}")
    }

    #[test]
    fn only_fresh_notifications_signed_with_our_secret_pass() {
        let (p, body, now) = (payment(), r#"{"type":"checkout.session.completed"}"#, 1_800_000_000);
        assert!(p.verify(&sign("whsec_essai", now, body), body.as_bytes(), now));
        assert!(p.verify(&format!("{},v1=00", sign("whsec_essai", now, body)), body.as_bytes(), now + 200), "plusieurs signatures, dans la tolérance");
        assert!(!p.verify(&sign("autre_secret", now, body), body.as_bytes(), now), "autre secret");
        assert!(!p.verify(&sign("whsec_essai", now, body), b"{\"type\":\"autre\"}", now), "corps modifié");
        assert!(!p.verify(&sign("whsec_essai", now, body), body.as_bytes(), now + 301), "trop ancienne");
        assert!(!p.verify("v1=abc", body.as_bytes(), now) && !p.verify("", body.as_bytes(), now), "sans horodatage");
    }
}
