//! Paliers et quotas : chaque compte a un palier qui fixe sa place de stockage et ses minutes
//! d'export par mois. Sans abonnement, un compte est au palier d'essai.
//!
//! Index : `sk = compte` porte le palier du client ; `sk = export#<aaaa-mm>#<nom>` porte la durée de
//! chaque export final du mois, dont la somme fait la consommation.
//!
//! Route (JSON) :
//!   GET /api/compte/palier → {plan, used_bytes, export_used_s, plans, payment}

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::account::Scope;
use crate::{aws, Fail};

/// Palier d'un compte sans abonnement.
pub const FREE: &str = "essai";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub key: String,
    pub label: String,
    /// Place de stockage, en Go.
    pub quota_go: f64,
    /// Minutes d'export final incluses par mois.
    pub export_min: f64,
    /// Prix payé par le client, en euros par an (0 : palier gratuit).
    pub eur_year: f64,
}

impl Plan {
    pub fn quota_bytes(&self) -> u64 {
        (self.quota_go * 1e9) as u64
    }
    pub fn export_s(&self) -> f64 {
        self.export_min * 60.0
    }
}

/// Grille par défaut (celle du cahier des charges) ; `--plans` la remplace.
pub fn defaults() -> Vec<Plan> {
    [(FREE, "Essai", 128.0, 10.0, 0.0), ("200go", "200 Go", 200.0, 15.0, 19.0), ("600go", "600 Go", 600.0, 30.0, 39.0),
     ("1to", "1 To", 1000.0, 60.0, 65.0), ("2to", "2 To", 2000.0, 120.0, 105.0)]
        .into_iter()
        .map(|(key, label, quota_go, export_min, eur_year)| Plan { key: key.into(), label: label.into(), quota_go, export_min, eur_year })
        .collect()
}

/// Grille lue dans un fichier JSON (liste de paliers) ; elle doit contenir le palier d'essai.
pub fn load(path: &Path) -> Result<Vec<Plan>> {
    let plans: Vec<Plan> = serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("lecture de {}", path.display()))?)
        .context("grille de paliers illisible")?;
    anyhow::ensure!(plans.iter().any(|p| p.key == FREE), "la grille doit contenir le palier « {FREE} »");
    Ok(plans)
}

/// Mois courant (UTC), clé de la consommation d'export.
pub fn month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

fn text(item: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    item.get(key).and_then(|v| v.as_s().ok()).cloned()
}

impl Scope {
    fn account_key(&self) -> (AttributeValue, AttributeValue) {
        (AttributeValue::S(format!("client#{}", self.client)), AttributeValue::S("compte".into()))
    }

    /// Palier du client (celui d'essai sans abonnement ou si son palier a quitté la grille).
    pub async fn plan(&self) -> Result<&Plan, Fail> {
        let free = self.plans.iter().find(|p| p.key == FREE).expect("palier d'essai présent");
        let Some(table) = &self.table else { return Ok(free) };
        let (pk, sk) = self.account_key();
        let got = self.db.get_item().table_name(table).key("pk", pk).key("sk", sk).send().await.map_err(aws("lecture du compte"))?;
        let key = got.item().and_then(|i| text(i, "palier"));
        Ok(key.and_then(|k| self.plans.iter().find(|p| p.key == k)).unwrap_or(free))
    }

    /// Place occupée par les rushs du client (les exports n'y comptent pas).
    pub async fn used_bytes(&self) -> Result<u64, Fail> {
        if self.table.is_none() {
            return Ok(0);
        }
        Ok(self.rows("rush#").await?.iter().filter_map(|i| i.get("octets")?.as_n().ok()?.parse::<f64>().ok()).sum::<f64>() as u64)
    }

    /// Secondes d'export final déjà consommées ce mois-ci.
    pub async fn export_used_s(&self) -> Result<f64, Fail> {
        if self.table.is_none() {
            return Ok(0.0);
        }
        Ok(self.rows(&format!("export#{}#", month())).await?.iter().filter_map(|i| i.get("secondes")?.as_n().ok()?.parse::<f64>().ok()).sum())
    }

    /// Refuse un envoi qui ferait dépasser la place du palier.
    pub async fn check_storage(&self, incoming: u64) -> Result<(), Fail> {
        let (plan, used) = (self.plan().await?, self.used_bytes().await?);
        if used + incoming > plan.quota_bytes() {
            return Err(Fail(StatusCode::PAYMENT_REQUIRED, format!(
                "place insuffisante : {:.1} Go utilisés sur {:.0} Go (palier {}), et ce fichier en pèse {:.1}",
                used as f64 / 1e9, plan.quota_go, plan.label, incoming as f64 / 1e9)));
        }
        Ok(())
    }

    /// Refuse un export final qui ferait dépasser les minutes du mois.
    pub async fn check_export(&self, seconds: f64) -> Result<(), Fail> {
        let (plan, used) = (self.plan().await?, self.export_used_s().await?);
        if used + seconds > plan.export_s() {
            return Err(Fail(StatusCode::PAYMENT_REQUIRED, format!(
                "quota d'export atteint : {:.1} min utilisées sur {} ce mois-ci (palier {}), et cet export en demande {:.1}",
                used / 60.0, plan.export_min, plan.label, seconds / 60.0)));
        }
        Ok(())
    }

    /// Compte un export final dans la consommation du mois.
    pub async fn record_export(&self, name: &str, seconds: f64, bytes: u64) -> Result<()> {
        let Some(table) = &self.table else { return Ok(()) };
        self.db.put_item().table_name(table)
            .item("pk", AttributeValue::S(format!("client#{}", self.client)))
            .item("sk", AttributeValue::S(format!("export#{}#{name}", month())))
            .item("secondes", AttributeValue::N(format!("{seconds:.1}")))
            .item("octets", AttributeValue::N(bytes.to_string()))
            .item("date", AttributeValue::S(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()))
            .send().await?;
        Ok(())
    }

    /// Fixe le palier du client (abonnement payé, ou retour à l'essai) et garde ses références de paiement.
    pub async fn set_plan(&self, key: &str, customer: Option<&str>, subscription: Option<&str>) -> Result<()> {
        let table = self.table.as_ref().context("index non configuré")?;
        let (pk, sk) = self.account_key();
        let mut put = self.db.put_item().table_name(table).item("pk", pk).item("sk", sk)
            .item("palier", AttributeValue::S(key.into()))
            .item("depuis", AttributeValue::S(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        for (attr, v) in [("paiement_client", customer), ("paiement_abonnement", subscription)] {
            if let Some(v) = v {
                put = put.item(attr, AttributeValue::S(v.into()));
            }
        }
        put.send().await?;
        Ok(())
    }
}

pub async fn status(c: Scope) -> Result<Json<Value>, Fail> {
    let plan = c.plan().await?;
    Ok(Json(json!({
        "plan": {"key": plan.key, "label": plan.label, "quota_bytes": plan.quota_bytes(), "export_s": plan.export_s()},
        "used_bytes": c.used_bytes().await?, "export_used_s": c.export_used_s().await?,
        "plans": c.plans, "payment": c.payment.is_some(),
    })))
}
