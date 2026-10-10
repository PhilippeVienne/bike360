//! Paiement : le client choisit un palier ou achète du crédit d'export, paie sur la page du
//! prestataire, et c'est la notification du prestataire (et elle seule) qui change son compte.
//!
//! Le prestataire est derrière l'interface `Provider`. Mollie (`mollie.rs`) est celui du service ;
//! Stripe (`stripe.rs`) reste disponible, sans les écrans de moyen de paiement.
//!
//! Réglages (variables d'environnement ou secrets lus au démarrage, jamais en ligne de commande) :
//!   BIKE360_PAYMENT   « mollie » ou « stripe » ; sans elle, le prestataire dont la clé est donnée, Mollie d'abord
//!   BIKE360_MOLLIE_KEY, ou BIKE360_STRIPE_KEY et les autres réglages de `stripe.rs`
//!   BIKE360_VENDEUR   identité du vendeur portée sur les reçus (nom, adresse, SIREN)
//!
//! Routes (JSON) :
//!   POST /api/paiement/commande {plan} → {url}       page de paiement pour ce palier ; des rushs
//!        archivés y ajoutent les frais de leur récupération
//!   POST /api/paiement/credit {minutes} → {url}      achat de minutes d'export, payées d'avance
//!   POST /api/paiement/resiliation {undo} → {ok, cancel_at}   l'abonnement s'arrêtera à son échéance (ou reprend)
//!   GET  /api/paiement/moyen → {method}              moyen de paiement sur lequel l'abonnement est prélevé
//!   POST /api/paiement/moyen → {url}                 page du prestataire pour en enregistrer un autre
//!   GET  /api/paiement/recus → {receipts, seller, vat}   historique des paiements du compte
//!   POST /api/paiement/mollie, /api/paiement/stripe  notification du prestataire
//!
//! Index : `pk = paiement#<id>`, `sk = recu` garde chaque paiement encaissé (il n'est appliqué
//! qu'une fois) ; `pk = client#<client>`, `sk = recu#<numéro>` en est la copie que le client lit.
//! Les reçus sont numérotés sans trou, par année (`pk = recus`, `sk = compteur#<année>`).

mod mollie;
mod stripe;

use std::collections::HashMap;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::{AttributeValue, Put, ReturnValue, TransactWriteItem, Update};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::account::Scope;
use crate::plans::{iso, Account, Plan, Standing, FREE};
use crate::{aws, Ctx, Fail};

pub use mollie::webhook as mollie_webhook;
pub use stripe::webhook as stripe_webhook;

/// Achat de crédit le plus gros accepté en une fois, en minutes.
const MAX_CREDIT_MIN: u32 = 6000;
/// Jours avant l'échéance entre lesquels part le rappel de reconduction : plus d'un mois, moins de trois.
const REMIND_DAYS: (f64, f64) = (31.0, 60.0);
/// Mention portée sur chaque reçu : le vendeur est en franchise en base de TVA.
const VAT_NOTICE: &str = "TVA non applicable, article 293 B du CGI";

/// Ce qu'un prestataire de paiement sait faire pour le service. Chaque appel renvoie soit la page
/// du prestataire où envoyer le client, soit ce que le prestataire a retenu.
pub trait Provider {
    fn name(&self) -> &'static str;
    /// Page de paiement d'un abonnement annuel à ce palier, frais de récupération compris s'il y en a.
    async fn order(&self, c: &Scope, acc: &Account, plan: &Plan, recovery_eur: Option<f64>) -> Result<String, Fail>;
    /// Page de paiement de minutes d'export.
    async fn credit(&self, c: &Scope, acc: &Account, minutes: u32) -> Result<String, Fail>;
    /// Résilie l'abonnement à son échéance (ou revient sur la résiliation) ; renvoie la date de fin.
    async fn cancel(&self, c: &Scope, acc: &Account, undo: bool) -> Result<Option<String>, Fail>;
    /// Arrête tout de suite ce que le prestataire garde du compte (effacement du compte, abonnement échu).
    async fn end_now(&self, acc: &Account) -> Result<(), Fail>;
    /// Moyen de paiement enregistré : {kind, label, status}.
    async fn method(&self, acc: &Account) -> Result<Option<Value>, Fail>;
    /// Page du prestataire où enregistrer un autre moyen de paiement.
    async fn change_method(&self, c: &Scope, acc: &Account) -> Result<String, Fail>;
}

pub enum Payment {
    Mollie(mollie::Mollie),
    Stripe(stripe::Stripe),
}

/// Appelle le prestataire configuré, quel qu'il soit.
macro_rules! provider {
    ($payment:expr, $p:ident => $call:expr) => {
        match $payment {
            Payment::Mollie($p) => $call,
            Payment::Stripe($p) => $call,
        }
    };
}

impl Payment {
    /// Réglages lus dans les secrets du service, sinon dans l'environnement ; None si le paiement n'est pas configuré.
    pub fn from_env(site: String, secrets: &HashMap<String, String>) -> Result<Option<Payment>> {
        let var = |k: &str| secrets.get(k).cloned().or_else(|| std::env::var(k).ok()).filter(|v| !v.is_empty());
        match var("BIKE360_PAYMENT").as_deref() {
            Some("mollie") => Ok(Some(Payment::Mollie(mollie::Mollie::from_env(site, &var)?.context("BIKE360_PAYMENT=mollie demande BIKE360_MOLLIE_KEY")?))),
            Some("stripe") => Ok(Some(Payment::Stripe(stripe::Stripe::from_env(site, &var)?.context("BIKE360_PAYMENT=stripe demande BIKE360_STRIPE_KEY et BIKE360_STRIPE_WEBHOOK_SECRET")?))),
            Some(other) => anyhow::bail!("BIKE360_PAYMENT : prestataire inconnu « {other} » (mollie ou stripe)"),
            None => Ok(match mollie::Mollie::from_env(site.clone(), &var)? {
                Some(m) => Some(Payment::Mollie(m)),
                None => stripe::Stripe::from_env(site, &var)?.map(Payment::Stripe),
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        provider!(self, p => p.name())
    }

    pub async fn end_now(&self, acc: &Account) -> Result<(), Fail> {
        provider!(self, p => p.end_now(acc).await)
    }
}

fn payment(c: &Ctx) -> Result<&Payment, Fail> {
    c.payment.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "le paiement n'est pas configuré sur ce service".into()))
}

/// Un identifiant venu du prestataire devient une clé de l'index : lettres, chiffres, - et _ seulement.
fn safe(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn cents(eur: f64) -> u64 {
    (eur * 100.0).round() as u64
}

#[derive(Deserialize)]
pub struct Order {
    plan: String,
}

/// Ouvre une page de paiement chez le prestataire pour ce palier, rattachée au compte connecté.
pub async fn order(c: Scope, Json(b): Json<Order>) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    let plan = c.plans.iter().find(|x| x.key == b.plan && x.key != FREE && x.eur_year > 0.0)
        .ok_or_else(|| Fail(StatusCode::BAD_REQUEST, "ce palier n'est pas en vente".into()))?;
    let acc = c.account().await?;
    // des rushs en archive reviennent avec l'abonnement : leur récupération est facturée avec lui
    let recovery = match c.standing_of(&acc) {
        Standing::Archived { .. } => Some(c.policy.recovery_eur(c.used_bytes().await?)),
        _ => None,
    };
    let url = provider!(p, x => x.order(&c, &acc, plan, recovery).await)?;
    Ok(Json(json!({"url": url})))
}

#[derive(Deserialize)]
pub struct Credit {
    minutes: u32,
}

/// Ouvre une page de paiement pour des minutes d'export, payées avant d'être utilisées.
pub async fn credit(c: Scope, Json(b): Json<Credit>) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    if b.minutes < c.policy.credit_min || b.minutes > MAX_CREDIT_MIN {
        return Err(Fail(StatusCode::BAD_REQUEST, format!("le crédit s'achète par {} minutes au moins ({MAX_CREDIT_MIN} au plus)", c.policy.credit_min)));
    }
    let acc = c.account().await?;
    let url = provider!(p, x => x.credit(&c, &acc, b.minutes).await)?;
    Ok(Json(json!({"url": url})))
}

#[derive(Deserialize)]
pub struct Cancel {
    /// Vrai pour revenir sur une résiliation demandée.
    #[serde(default)]
    undo: bool,
}

/// Résilie l'abonnement : il court jusqu'à son échéance, déjà payée, puis ne se renouvelle pas.
pub async fn cancel(c: Scope, Json(b): Json<Cancel>) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    let acc = c.account().await?;
    if acc.subscription.is_none() {
        return Err(Fail(StatusCode::CONFLICT, "ce compte n'a pas d'abonnement en cours".into()));
    }
    let at = provider!(p, x => x.cancel(&c, &acc, b.undo).await)?;
    // sans date annoncée, la demande est tout de même retenue
    let kept = (!b.undo).then(|| at.clone().unwrap_or_else(|| iso(Utc::now())));
    c.patch(&[("resiliation", kept.map(AttributeValue::S))]).await.map_err(|e| {
        eprintln!("résiliation : {e:#}");
        Fail(StatusCode::INTERNAL_SERVER_ERROR, "résiliation enregistrée chez le prestataire, mais pas dans le compte".into())
    })?;
    Ok(Json(json!({"ok": true, "cancel_at": at.filter(|_| !b.undo)})))
}

/// Moyen de paiement sur lequel l'abonnement du compte est prélevé.
pub async fn method(c: Scope) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    let acc = c.account().await?;
    Ok(Json(json!({"method": provider!(p, x => x.method(&acc).await)?})))
}

/// Ouvre la page du prestataire où enregistrer un autre moyen de paiement.
pub async fn change_method(c: Scope) -> Result<Json<Value>, Fail> {
    let p = payment(&c)?;
    let acc = c.account().await?;
    Ok(Json(json!({"url": provider!(p, x => x.change_method(&c, &acc).await)?})))
}

/// Historique des paiements du compte, du plus récent au plus ancien, avec ce qu'un reçu doit porter.
pub async fn receipts(c: Scope) -> Result<Json<Value>, Fail> {
    let text = |i: &HashMap<String, AttributeValue>, k: &str| i.get(k).and_then(|v| v.as_s().ok()).cloned();
    let mut list: Vec<Value> = if c.table.is_some() { c.rows("recu#").await? } else { vec![] }.iter().map(|i| json!({
        "number": text(i, "numero"), "date": text(i, "date"), "kind": text(i, "nature"), "label": text(i, "libelle"),
        "cents": i.get("centimes").and_then(|v| v.as_n().ok()).and_then(|n| n.parse::<u64>().ok()),
        "method": text(i, "moyen"), "state": text(i, "etat"), "reference": text(i, "reference"),
    })).collect();
    list.reverse();
    Ok(Json(json!({"receipts": list, "seller": c.seller, "vat": VAT_NOTICE})))
}

/// Un paiement encaissé, tel qu'il est gardé et montré au client.
pub struct Receipt<'a> {
    /// Identifiant du paiement chez le prestataire.
    pub id: &'a str,
    pub client: &'a str,
    /// « abonnement », « renouvellement » ou « credit ».
    pub kind: &'a str,
    pub label: String,
    pub cents: u64,
    /// Moyen de paiement, tel que le prestataire le nomme.
    pub method: Option<&'a str>,
    /// Secondes de crédit d'export que ce paiement ajoute au compte.
    pub credit_s: f64,
}

fn receipt_key(id: &str) -> (AttributeValue, AttributeValue) {
    (AttributeValue::S(format!("paiement#{id}")), AttributeValue::S("recu".into()))
}

/// Ligne de l'index gardée pour un paiement déjà encaissé.
async fn recorded(ctx: &Ctx, table: &str, id: &str) -> Result<Option<HashMap<String, AttributeValue>>> {
    let (pk, sk) = receipt_key(id);
    Ok(ctx.db.get_item().table_name(table).key("pk", pk).key("sk", sk).consistent_read(true).send().await?.item)
}

/// Garde un paiement encaissé, lui donne son numéro de reçu et ajoute le crédit qu'il porte : tout ou
/// rien, et une seule fois par paiement (le prestataire peut représenter une notification).
pub async fn record(ctx: &Ctx, r: &Receipt<'_>) -> Result<bool> {
    let table = ctx.table.as_ref().context("index non configuré")?;
    let s = |v: &str| AttributeValue::S(v.to_string());
    let now = Utc::now();
    let counter = (s("recus"), s(&format!("compteur#{}", now.year())));
    for _ in 0..8 {
        if recorded(ctx, table, r.id).await?.is_some() {
            return Ok(false);
        }
        let last = ctx.db.get_item().table_name(table).key("pk", counter.0.clone()).key("sk", counter.1.clone()).consistent_read(true).send().await?
            .item.and_then(|i| i.get("n")?.as_n().ok()?.parse::<u64>().ok()).unwrap_or(0);
        let number = format!("{}-{:06}", now.year(), last + 1);
        let fields = |put: aws_sdk_dynamodb::types::builders::PutBuilder| {
            let put = put.table_name(table).item("numero", s(&number)).item("date", s(&iso(now))).item("client", s(r.client))
                .item("nature", s(r.kind)).item("libelle", s(&r.label)).item("centimes", AttributeValue::N(r.cents.to_string()))
                .item("etat", s("encaisse")).item("reference", s(r.id)).item("secondes", AttributeValue::N(format!("{:.0}", r.credit_s)));
            match r.method {
                Some(m) => put.item("moyen", s(m)),
                None => put,
            }
        };
        let (pk, sk) = receipt_key(r.id);
        let mut tx = ctx.db.transact_write_items()
            .transact_items(TransactWriteItem::builder().update(Update::builder().table_name(table).key("pk", counter.0.clone()).key("sk", counter.1.clone())
                .update_expression("SET n = :new").condition_expression("attribute_not_exists(n) OR n = :old")
                .expression_attribute_values(":new", AttributeValue::N((last + 1).to_string()))
                .expression_attribute_values(":old", AttributeValue::N(last.to_string())).build()?).build())
            .transact_items(TransactWriteItem::builder().put(fields(Put::builder()).item("pk", pk).item("sk", sk)
                .condition_expression("attribute_not_exists(pk)").build()?).build())
            .transact_items(TransactWriteItem::builder().put(fields(Put::builder()).item("pk", s(&format!("client#{}", r.client)))
                .item("sk", s(&format!("recu#{number}"))).build()?).build());
        if r.credit_s > 0.0 {
            tx = tx.transact_items(TransactWriteItem::builder().update(Update::builder().table_name(table)
                .key("pk", s(&format!("client#{}", r.client))).key("sk", s("compte"))
                .update_expression("ADD credit_s :s").expression_attribute_values(":s", AttributeValue::N(format!("{:.0}", r.credit_s))).build()?).build());
        }
        match tx.send().await {
            Ok(_) => return Ok(true),
            // un autre paiement a pris ce numéro, ou la même notification est arrivée deux fois : on regarde de nouveau
            Err(e) if e.as_service_error().is_some_and(|s| s.is_transaction_canceled_exception()) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("numéro de reçu disputé pour le paiement {}", r.id)
}

/// Un paiement encaissé puis repris (remboursement complet, contestation) : marqué une seule fois.
/// Renvoie alors son client, sa nature et le crédit qu'il avait ajouté.
async fn reverse(ctx: &Ctx, id: &str) -> Result<Option<(String, String, f64)>> {
    let table = ctx.table.as_ref().context("index non configuré")?;
    let (pk, sk) = receipt_key(id);
    let s = |v: &str| AttributeValue::S(v.to_string());
    let done = ctx.db.update_item().table_name(table).key("pk", pk).key("sk", sk)
        .update_expression("SET etat = :r").condition_expression("etat = :e")
        .expression_attribute_values(":r", s("repris")).expression_attribute_values(":e", s("encaisse"))
        .return_values(ReturnValue::AllNew).send().await;
    let item = match done {
        Ok(out) => out.attributes.unwrap_or_default(),
        Err(e) if e.as_service_error().is_some_and(|s| s.is_conditional_check_failed_exception()) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let text = |k: &str| item.get(k).and_then(|v| v.as_s().ok()).cloned().unwrap_or_default();
    let (client, number) = (text("client"), text("numero"));
    ctx.db.update_item().table_name(table).key("pk", s(&format!("client#{client}"))).key("sk", s(&format!("recu#{number}")))
        .update_expression("SET etat = :r").condition_expression("attribute_exists(pk)").expression_attribute_values(":r", s("repris")).send().await.ok();
    let seconds = item.get("secondes").and_then(|v| v.as_n().ok()).and_then(|n| n.parse().ok()).unwrap_or(0.0);
    Ok(Some((client, text("nature"), seconds)))
}

fn parse(date: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(date).ok().map(|t| t.with_timezone(&Utc))
}

/// Fin d'un abonnement dont le service suit lui-même l'échéance (`echeance` dans la fiche) : à
/// l'échéance s'il est résilié, ou quand son renouvellement n'est toujours pas payé au bout du délai
/// de grâce. Vrai si le compte vient de revenir au palier d'essai.
pub async fn lapse(c: &Scope, acc: &Account) -> Result<bool> {
    let Some(until) = acc.paid_until.as_deref().and_then(parse) else { return Ok(false) };
    if c.plan_of(acc).key == FREE {
        return Ok(false);
    }
    let now = Utc::now();
    // reconduction tacite : le client en est prévenu entre trois mois et un mois avant l'échéance
    // (article L215-1 du code de la consommation), une fois par échéance
    let days_left = (until - now).num_seconds() as f64 / 86400.0;
    if acc.cancel_at.is_none() && days_left > REMIND_DAYS.0 && days_left <= REMIND_DAYS.1 && acc.reminded != acc.paid_until {
        let plan = c.plan_of(acc);
        let sent = c.mail("Bike360 : ton abonnement se renouvelle bientôt", &format!(
            "Bonjour,\n\nTon abonnement Bike360 Cloud (palier {}) se renouvellera le {} pour un an, au prix de {} €, prélevés sur ton moyen de paiement.\n\n\
             Tu n'as rien à faire pour le garder. Pour ne pas le reconduire, tu peux le résilier jusqu'à cette date, sans frais, depuis ta page de palier :\n\n\
             {}/ui/palier.html\n", plan.label, until.format("%d/%m/%Y"), plan.eur_year, c.site)).await;
        match sent {
            Ok(true) => {
                c.patch(&[("rappel", acc.paid_until.clone().map(AttributeValue::S))]).await?;
                println!("rappel de reconduction envoyé à {}", c.client);
            }
            Ok(false) => {}
            Err(e) => eprintln!("rappel de reconduction à {} : {e:#}", c.client),
        }
    }
    if acc.cancel_at.is_some() {
        if now < until {
            return Ok(false);
        }
        c.unsubscribed().await?;
        println!("abonnement de {} résilié : arrivé à son échéance", c.client);
        return Ok(true);
    }
    if now < until + chrono::Duration::seconds((c.policy.grace_days * 86400.0) as i64) {
        return Ok(false);
    }
    // le prestataire a renoncé à prélever : plus rien ne doit partir, et le compte perd son palier
    if let Some(p) = &c.payment {
        if let Err(e) = provider!(p, x => x.cancel(c, acc, false).await) {
            eprintln!("arrêt de l'abonnement impayé de {} : {}", c.client, e.1);
        }
    }
    c.unsubscribed().await?;
    println!("abonnement de {} terminé : renouvellement impayé depuis {} jours", c.client, c.policy.grace_days);
    let sent = c.mail("Bike360 : ton abonnement est terminé", &format!(
        "Bonjour,\n\nLe renouvellement de ton abonnement n'a pas pu être prélevé. Ton abonnement est terminé : tes rushs restent \
         accessibles {} jours, puis partent en archive. Reprends un palier pour envoyer de nouveau.\n\n{}/ui/palier.html\n",
        c.policy.access_days, c.site)).await;
    if let Err(e) = sent {
        eprintln!("courriel de fin d'abonnement à {} : {e:#}", c.client);
    }
    Ok(true)
}

/// Lit la ligne d'index `pk`, `sk` (lecture cohérente : une notification suit de près ce qui l'a provoquée).
async fn row(ctx: &Ctx, pk: String, sk: &str) -> Result<Option<HashMap<String, AttributeValue>>, Fail> {
    let Some(table) = &ctx.table else { return Ok(None) };
    Ok(ctx.db.get_item().table_name(table).key("pk", AttributeValue::S(pk)).key("sk", AttributeValue::S(sk.into()))
        .consistent_read(true).send().await.map_err(aws("lecture de l'index"))?.item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_round_to_the_cent_and_ids_stay_plain() {
        assert_eq!(cents(19.0), 1900);
        assert_eq!(cents(0.05 * 60.0), 300);
        assert_eq!(cents(0.1 + 0.2), 30);
        assert!(safe("tr_5B8cwPMGnU") && safe("cst_x-1"));
        assert!(!safe("") && !safe("tr_../x") && !safe("tr_é") && !safe(&"a".repeat(65)));
    }
}
