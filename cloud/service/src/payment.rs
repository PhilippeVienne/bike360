//! Paiement par Stripe : le client choisit un palier, paie sur la page de Stripe, et c'est la
//! notification signée de Stripe (et elle seule) qui change le palier du compte.
//!
//! Réglages (variables d'environnement, jamais en ligne de commande) : BIKE360_STRIPE_KEY,
//! BIKE360_STRIPE_WEBHOOK_SECRET, BIKE360_STRIPE_PRICES (« 600go=price_…,1to=price_… »).
//!
//! Routes (JSON) :
//!   POST /api/paiement/commande {plan} → {url}   page de paiement de Stripe pour ce palier
//!   POST /api/paiement/stripe             notification de Stripe (corps brut, en-tête Stripe-Signature)

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
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::account::Scope;
use crate::plans::FREE;
use crate::{Ctx, Fail};

/// Écart toléré entre l'heure d'une notification et la nôtre (contre le rejeu).
const TOLERANCE_S: u64 = 300;

pub struct Payment {
    secret_key: String,
    webhook_secret: String,
    /// Adresse de l'API de Stripe (remplaçable par un émulateur pour les essais).
    api: String,
    /// Palier → identifiant de prix chez Stripe.
    prices: HashMap<String, String>,
    /// Adresse publique du site, où Stripe renvoie le client après le paiement.
    site: String,
}

impl Payment {
    /// Réglages lus dans l'environnement ; None si le paiement n'est pas configuré.
    pub fn from_env(site: String) -> Result<Option<Payment>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let (Some(secret_key), Some(webhook_secret)) = (var("BIKE360_STRIPE_KEY"), var("BIKE360_STRIPE_WEBHOOK_SECRET")) else { return Ok(None) };
        let prices: HashMap<String, String> = var("BIKE360_STRIPE_PRICES").unwrap_or_default().split(',')
            .filter_map(|p| p.trim().split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        anyhow::ensure!(!prices.is_empty(), "BIKE360_STRIPE_PRICES est vide : aucun palier n'est vendable");
        let api = var("BIKE360_STRIPE_API").unwrap_or_else(|| "https://api.stripe.com".into());
        Ok(Some(Payment { secret_key, webhook_secret, api: api.trim_end_matches('/').to_string(), prices, site }))
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
}

fn payment(c: &Ctx) -> Result<&Payment, Fail> {
    c.payment.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "le paiement n'est pas configuré sur ce service".into()))
}

#[derive(Deserialize)]
pub struct Order {
    plan: String,
}

/// Ouvre une page de paiement chez Stripe pour ce palier, rattachée au compte connecté.
pub async fn order(c: Scope, Json(b): Json<Order>) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    let price = p.prices.get(&b.plan).filter(|_| c.plans.iter().any(|x| x.key == b.plan && x.key != FREE))
        .ok_or_else(|| Fail(StatusCode::BAD_REQUEST, "ce palier n'est pas en vente".into()))?.clone();
    let (url, key, client, plan) = (format!("{}/v1/checkout/sessions", p.api), p.secret_key.clone(), c.client.clone(), b.plan.clone());
    let (ok_url, back_url) = (format!("{}/ui/palier.html?paiement=ok", p.site), format!("{}/ui/palier.html", p.site));
    let session = tokio::task::spawn_blocking(move || -> Result<Value> {
        Ok(ureq::post(&url).set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(20)).send_form(&[
            ("mode", "subscription"), ("line_items[0][price]", &price), ("line_items[0][quantity]", "1"),
            ("client_reference_id", &client), ("metadata[palier]", &plan),
            ("success_url", &ok_url), ("cancel_url", &back_url),
        ])?.into_json()?)
    }).await.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r).map_err(|e| {
        eprintln!("commande : {e:#}");
        Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement ne répond pas".into())
    })?;
    let url = session["url"].as_str().ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "le prestataire de paiement n'a pas renvoyé de page".into()))?;
    Ok(Json(json!({"url": url})))
}

fn safe(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Applique une notification déjà authentifiée : paiement abouti → palier ; abonnement terminé → essai.
async fn apply(ctx: &Arc<Ctx>, event: &Value) -> Result<&'static str> {
    let table = ctx.table.as_ref().context("index non configuré")?;
    let obj = &event["data"]["object"];
    match event["type"].as_str().unwrap_or_default() {
        "checkout.session.completed" => {
            let (Some(client), Some(plan)) = (obj["client_reference_id"].as_str().filter(|c| safe(c)), obj["metadata"]["palier"].as_str()) else {
                return Ok("ignorée : commande sans compte ni palier");
            };
            if !ctx.plans.iter().any(|p| p.key == plan && p.key != FREE) {
                return Ok("ignorée : palier inconnu");
            }
            if !matches!(obj["payment_status"].as_str(), Some("paid" | "no_payment_required")) {
                return Ok("ignorée : paiement non abouti");
            }
            let customer = obj["customer"].as_str();
            Scope::of(ctx.clone(), client.to_string()).set_plan(plan, customer, obj["subscription"].as_str()).await?;
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
            Scope::of(ctx.clone(), client).set_plan(FREE, Some(customer), None).await?;
            Ok("retour au palier d'essai")
        }
        _ => Ok("ignorée : type non traité"),
    }
}

/// Notification de Stripe. Sans signature valable, rien n'est lu ni appliqué.
pub async fn webhook(State(ctx): State<Arc<Ctx>>, headers: HeaderMap, body: Bytes) -> Result<Json<Value>, Fail> {
    let p = payment(&ctx)?;
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

    fn payment() -> Payment {
        Payment { secret_key: "sk".into(), webhook_secret: "whsec_essai".into(), api: String::new(), prices: HashMap::new(), site: String::new() }
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
