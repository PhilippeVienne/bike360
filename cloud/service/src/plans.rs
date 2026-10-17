//! Paliers et quotas : chaque compte a un palier qui fixe sa place de stockage et ses minutes
//! d'export par mois. Sans abonnement, un compte est au palier d'essai, qui ne dure qu'un temps.
//!
//! Index : `sk = compte` porte la fiche du client (palier, dates de l'essai et de l'abonnement,
//! crédit d'export) ; `sk = export#<aaaa-mm>#<nom>` porte la durée de chaque export final du mois,
//! dont la somme fait la consommation.
//!
//! Situation d'un compte (voir `Standing`) :
//!   essai       du premier envoi à `trial_days` ; ensuite ses rushs sont supprimés
//!   abonné      tant que l'abonnement court
//!   terminé     `access_days` après la fin de l'abonnement : tout reste accessible, sans nouvel envoi
//!   archivé     ensuite, `archive_days` en archive profonde : la récupération est payante
//!   supprimé    au-delà, les rushs n'existent plus
//!
//! Route (JSON) :
//!   GET /api/compte/palier → {plan, used_bytes, export_used_s, credit_s, standing, plans, payment, …}

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
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
    [(FREE, "Essai", 128.0, 10.0, 0.0), ("200go", "200 Go", 200.0, 15.0, 25.0), ("600go", "600 Go", 600.0, 30.0, 49.0)]
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

/// Délais et tarifs hors grille : durée de l'essai, sort des rushs après un abonnement, crédit d'export.
pub struct Policy {
    /// Jours d'essai à compter du premier envoi ; ensuite les rushs du compte sont supprimés.
    pub trial_days: f64,
    /// Jours d'accès après la fin d'un abonnement.
    pub access_days: f64,
    /// Jours de garde en archive profonde après cet accès, avant suppression.
    pub archive_days: f64,
    /// Achat minimal de crédit d'export, en minutes.
    pub credit_min: u32,
    /// Prix d'une minute de crédit d'export, en euros.
    pub credit_eur: f64,
    /// Prix de la récupération de rushs archivés, en euros par tranche de 100 Go.
    pub recovery_eur_100go: f64,
    /// Jours laissés à un renouvellement en retard avant de mettre fin à l'abonnement.
    pub grace_days: f64,
    /// Crédits (minutes d'export) que coûtent 100 Go au-dessus du quota pendant 30 jours.
    pub overage_credits_100go: f64,
    /// Jours de dépassement que le crédit doit couvrir pour qu'un envoi au-delà du quota soit accepté.
    pub overage_min_days: f64,
    /// Jours pour régulariser un dépassement une fois le crédit épuisé ; ensuite l'excédent le plus ancien part en archive.
    pub overage_grace_days: f64,
}

impl Policy {
    /// Crédit (en secondes d'export) que coûtent `bytes` octets au-dessus du quota pendant `days` jours.
    pub fn overage_s(&self, bytes: u64, days: f64) -> f64 {
        bytes as f64 / 100e9 * self.overage_credits_100go * 60.0 * days / 30.0
    }

    /// Crédit (en secondes d'export) que coûte la récupération de `bytes` octets partis en archive pour dépassement.
    pub fn recovery_s(&self, bytes: u64) -> f64 {
        if self.credit_eur <= 0.0 { 0.0 } else { (self.recovery_eur(bytes) / self.credit_eur).ceil() * 60.0 }
    }

    /// Prix de la récupération de `bytes` octets archivés, par tranches de 100 Go entamées.
    pub fn recovery_eur(&self, bytes: u64) -> f64 {
        (bytes as f64 / 100e9).ceil().max(1.0) * self.recovery_eur_100go
    }
}

/// Mois courant (UTC), clé de la consommation d'export.
pub fn month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) fn days_between(from: &str, now: DateTime<Utc>) -> f64 {
    DateTime::parse_from_rfc3339(from).map_or(0.0, |t| (now - t.with_timezone(&Utc)).num_seconds() as f64 / 86400.0)
}

fn text(item: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    item.get(key).and_then(|v| v.as_s().ok()).cloned()
}

/// Fiche d'un compte, telle que l'index la garde.
#[derive(Default)]
pub struct Account {
    plan: Option<String>,
    /// Premier envoi d'un compte à l'essai : le délai de l'essai court de là.
    pub trial_start: Option<String>,
    /// Fin du dernier abonnement, tant que le compte n'en a pas repris un.
    pub ended: Option<String>,
    /// Résiliation demandée : date à laquelle l'abonnement s'arrêtera.
    pub cancel_at: Option<String>,
    /// Rushs partis en archive profonde.
    pub archived: Option<String>,
    /// Rushs supprimés (fin d'essai ou fin de garde en archive).
    pub purged: Option<String>,
    /// Récupération payée, en cours.
    pub restoring: Option<String>,
    /// Crédit d'export acheté d'avance (s), consommé au-delà des minutes du mois.
    pub credit_s: f64,
    pub customer: Option<String>,
    pub subscription: Option<String>,
    /// Mandat de paiement sur lequel l'abonnement est prélevé.
    pub mandate: Option<String>,
    /// Échéance payée : l'abonnement est réglé jusqu'à cette date (prestataire qui n'annonce pas sa fin).
    pub paid_until: Option<String>,
    /// Renouvellement refusé par la banque, tant qu'il n'est pas régularisé.
    pub failed: Option<String>,
    /// Échéance pour laquelle le rappel de reconduction a été envoyé.
    pub reminded: Option<String>,
    /// Dépassement du quota de stockage : dernier décompte de crédit.
    pub over_seen: Option<String>,
    /// Dépassement du quota de stockage : crédit épuisé depuis cette date.
    pub over_out: Option<String>,
}

/// Ce qu'un compte peut faire aujourd'hui.
#[derive(Debug, PartialEq)]
pub enum Standing {
    /// À l'essai ; les jours restants ne courent qu'à partir du premier envoi.
    Trial { days_left: Option<f64> },
    TrialOver,
    Paid,
    /// Abonnement terminé : accès à tout sauf à l'envoi, pendant `days_left` jours.
    Ended { days_left: f64 },
    /// Rushs en archive profonde, supprimés dans `days_left` jours.
    Archived { days_left: f64 },
    /// Abonnement terminé et rushs supprimés.
    Purged,
    /// Abonnement repris, rushs en cours de retour de l'archive.
    Restoring,
}

impl Account {
    pub fn standing(&self, paid: bool, p: &Policy, now: DateTime<Utc>) -> Standing {
        if paid {
            return if self.restoring.is_some() { Standing::Restoring } else { Standing::Paid };
        }
        if let Some(end) = &self.ended {
            let days = days_between(end, now);
            return if self.purged.is_some() {
                Standing::Purged
            } else if self.archived.is_some() || days >= p.access_days {
                Standing::Archived { days_left: (p.access_days + p.archive_days - days).max(0.0) }
            } else {
                Standing::Ended { days_left: p.access_days - days }
            };
        }
        match &self.trial_start {
            Some(t) if days_between(t, now) >= p.trial_days => Standing::TrialOver,
            t => Standing::Trial { days_left: t.as_ref().map(|t| p.trial_days - days_between(t, now)) },
        }
    }
}

impl Standing {
    fn key(&self) -> &'static str {
        match self {
            Standing::Trial { .. } => "essai",
            Standing::TrialOver => "essai-termine",
            Standing::Paid => "abonne",
            Standing::Ended { .. } => "termine",
            Standing::Archived { .. } => "archive",
            Standing::Purged => "supprime",
            Standing::Restoring => "recuperation",
        }
    }

    /// Ce que le client doit savoir de sa situation (rien pour un abonné).
    fn notice(&self) -> Option<String> {
        let days = |d: &f64| format!("{} jour(s)", d.ceil().max(1.0));
        Some(match self {
            Standing::Paid | Standing::Trial { days_left: None } => return None,
            Standing::Trial { days_left: Some(d) } => format!("Essai gratuit : encore {}. Sans abonnement, tes rushs seront ensuite supprimés.", days(d)),
            Standing::TrialOver => "Essai terminé : les rushs envoyés pendant l'essai sont supprimés. Choisis un palier pour continuer.".into(),
            Standing::Ended { days_left } => format!("Abonnement terminé : tes rushs restent accessibles encore {}, puis partent en archive. Reprends un palier pour envoyer de nouveau.", days(days_left)),
            Standing::Archived { days_left } => format!("Abonnement terminé : tes rushs sont en archive, supprimés dans {}. Reprendre un palier les récupère, moyennant des frais de récupération.", days(days_left)),
            Standing::Purged => "Abonnement terminé : tes rushs ont été supprimés à la fin de leur garde en archive.".into(),
            Standing::Restoring => "Récupération en cours : tes rushs reviennent de l'archive, ce qui peut demander jusqu'à 48 h.".into(),
        })
    }

    pub fn json(&self) -> Value {
        let days_left = match self {
            Standing::Trial { days_left } => *days_left,
            Standing::Ended { days_left } | Standing::Archived { days_left } => Some(*days_left),
            _ => None,
        };
        json!({"state": self.key(), "days_left": days_left.map(|d| d.ceil()), "notice": self.notice()})
    }
}

fn refused(text: &str) -> Fail {
    Fail(StatusCode::PAYMENT_REQUIRED, text.into())
}

impl Scope {
    fn account_key(&self) -> (AttributeValue, AttributeValue) {
        (AttributeValue::S(format!("client#{}", self.client)), AttributeValue::S("compte".into()))
    }

    /// Fiche du compte (vide tant qu'il n'a rien envoyé ni payé).
    pub async fn account(&self) -> Result<Account, Fail> {
        let Some(table) = &self.table else { return Ok(Account::default()) };
        let (pk, sk) = self.account_key();
        let got = self.db.get_item().table_name(table).key("pk", pk).key("sk", sk).send().await.map_err(aws("lecture du compte"))?;
        let Some(i) = got.item() else { return Ok(Account::default()) };
        Ok(Account {
            plan: text(i, "palier"), trial_start: text(i, "essai_debut"), ended: text(i, "fin_abonnement"), cancel_at: text(i, "resiliation"),
            archived: text(i, "archive"), purged: text(i, "purge"), restoring: text(i, "recuperation"),
            credit_s: i.get("credit_s").and_then(|v| v.as_n().ok()).and_then(|n| n.parse().ok()).unwrap_or(0.0),
            customer: text(i, "paiement_client"), subscription: text(i, "paiement_abonnement"),
            mandate: text(i, "paiement_mandat"), paid_until: text(i, "echeance"), failed: text(i, "paiement_echec"), reminded: text(i, "rappel"),
            over_seen: text(i, "depassement_vu"), over_out: text(i, "depassement_fin"),
        })
    }

    /// Modifie la fiche du compte : `Some` pose l'attribut, `None` le retire.
    pub async fn patch(&self, changes: &[(&str, Option<AttributeValue>)]) -> Result<()> {
        let table = self.table.as_ref().context("index non configuré")?;
        let (pk, sk) = self.account_key();
        let mut up = self.db.update_item().table_name(table).key("pk", pk).key("sk", sk);
        let (mut set, mut remove) = (vec![], vec![]);
        for (n, (attr, value)) in changes.iter().enumerate() {
            up = up.expression_attribute_names(format!("#a{n}"), *attr);
            match value {
                Some(v) => {
                    up = up.expression_attribute_values(format!(":v{n}"), v.clone());
                    set.push(format!("#a{n} = :v{n}"));
                }
                None => remove.push(format!("#a{n}")),
            }
        }
        let mut expr = String::new();
        if !set.is_empty() {
            expr += &format!("SET {} ", set.join(", "));
        }
        if !remove.is_empty() {
            expr += &format!("REMOVE {}", remove.join(", "));
        }
        up.update_expression(expr.trim()).send().await?;
        Ok(())
    }

    /// Palier d'une fiche (celui d'essai sans abonnement ou si son palier a quitté la grille).
    pub fn plan_of(&self, acc: &Account) -> &Plan {
        let free = self.plans.iter().find(|p| p.key == FREE).expect("palier d'essai présent");
        acc.plan.as_ref().and_then(|k| self.plans.iter().find(|p| p.key == *k)).unwrap_or(free)
    }

    pub fn standing_of(&self, acc: &Account) -> Standing {
        acc.standing(self.plan_of(acc).key != FREE, &self.policy, Utc::now())
    }

    pub async fn plan(&self) -> Result<&Plan, Fail> {
        Ok(self.plan_of(&self.account().await?))
    }

    /// Place occupée par les rushs du client (les exports n'y comptent pas, ni les rushs partis en
    /// archive pour dépassement), puis place de ces rushs archivés.
    pub async fn storage(&self) -> Result<(u64, u64), Fail> {
        if self.table.is_none() {
            return Ok((0, 0));
        }
        let (mut live, mut frozen) = (0.0, 0.0);
        for i in self.rows("rush#").await? {
            let bytes = i.get("octets").and_then(|v| v.as_n().ok()).and_then(|n| n.parse::<f64>().ok()).unwrap_or(0.0);
            if i.contains_key("gele") { frozen += bytes } else { live += bytes }
        }
        Ok((live as u64, frozen as u64))
    }

    pub async fn used_bytes(&self) -> Result<u64, Fail> {
        Ok(self.storage().await?.0)
    }

    /// Secondes d'export final déjà consommées ce mois-ci.
    pub async fn export_used_s(&self) -> Result<f64, Fail> {
        if self.table.is_none() {
            return Ok(0.0);
        }
        Ok(self.rows(&format!("export#{}#", month())).await?.iter().filter_map(|i| i.get("secondes")?.as_n().ok()?.parse::<f64>().ok()).sum())
    }

    /// Refuse un envoi que la situation du compte interdit ou qui ferait dépasser la place du palier.
    /// Le premier envoi d'un compte à l'essai lance le délai de l'essai.
    pub async fn check_storage(&self, incoming: u64) -> Result<(), Fail> {
        let acc = self.account().await?;
        let standing = self.standing_of(&acc);
        match standing {
            Standing::Trial { .. } | Standing::Paid | Standing::Restoring => {}
            Standing::TrialOver => return Err(refused("essai terminé : choisis un palier pour envoyer de nouveaux rushs")),
            _ => return Err(refused("abonnement terminé : reprends un palier pour envoyer de nouveaux rushs")),
        }
        let (plan, used) = (self.plan_of(&acc), self.used_bytes().await?);
        if used + incoming > plan.quota_bytes() {
            let full = format!("place insuffisante : {:.1} Go utilisés sur {:.0} Go (palier {}), et ce fichier en pèse {:.1}",
                               used as f64 / 1e9, plan.quota_go, plan.label, incoming as f64 / 1e9);
            // un abonné dépasse son quota pour un temps si son crédit couvre le dépassement quelques jours
            let need = self.policy.overage_s(used + incoming - plan.quota_bytes(), self.policy.overage_min_days);
            if standing != Standing::Paid {
                return Err(Fail(StatusCode::PAYMENT_REQUIRED, full));
            }
            if acc.over_out.is_some() {
                return Err(Fail(StatusCode::PAYMENT_REQUIRED, format!("{full} ; ton crédit est épuisé : libère de la place ou rachète du crédit")));
            }
            if acc.credit_s < need {
                return Err(Fail(StatusCode::PAYMENT_REQUIRED, format!(
                    "{full} ; pour dépasser le quota il faut au moins {:.0} crédit(s), tu en as {:.0} ({:.0} crédits par 100 Go et par 30 jours)",
                    (need / 60.0).ceil(), (acc.credit_s / 60.0).floor(), self.policy.overage_credits_100go)));
            }
        }
        if standing == (Standing::Trial { days_left: None }) && self.table.is_some() {
            self.patch(&[("essai_debut", Some(AttributeValue::S(iso(Utc::now()))))]).await.map_err(aws("début de l'essai"))?;
        }
        Ok(())
    }

    /// Refuse l'accès aux rushs (atelier, export) quand ils ne sont plus là ou pas encore revenus.
    pub async fn check_access(&self) -> Result<Account, Fail> {
        let acc = self.account().await?;
        match self.standing_of(&acc) {
            Standing::Trial { .. } | Standing::Paid | Standing::Ended { .. } => Ok(acc),
            Standing::TrialOver => Err(refused("essai terminé : choisis un palier pour continuer")),
            Standing::Restoring => Err(Fail(StatusCode::CONFLICT, "récupération en cours : tes rushs reviennent de l'archive (jusqu'à 48 h)".into())),
            Standing::Archived { .. } | Standing::Purged => Err(refused("abonnement terminé : tes rushs ne sont plus accessibles")),
        }
    }

    /// Refuse un export final plus long que ce qu'il reste au compte : minutes du mois, puis crédit
    /// acheté d'avance. Un export ne commence jamais sans être couvert en entier.
    pub async fn check_export(&self, seconds: f64) -> Result<(), Fail> {
        let acc = self.check_access().await?;
        let (plan, used) = (self.plan_of(&acc), self.export_used_s().await?);
        let left = (plan.export_s() - used).max(0.0) + acc.credit_s;
        if seconds > left {
            return Err(Fail(StatusCode::PAYMENT_REQUIRED, format!(
                "quota d'export atteint : {:.1} min utilisées sur {} ce mois-ci (palier {}), {:.1} min de crédit, et cet export en demande {:.1} : il manque {:.0} min de crédit",
                used / 60.0, plan.export_min, plan.label, acc.credit_s / 60.0, seconds / 60.0, ((seconds - left) / 60.0).ceil())));
        }
        Ok(())
    }

    /// Compte un export final dans la consommation du mois ; ce qui dépasse les minutes du palier est pris
    /// sur le crédit. Un export déjà compté (même fichier présenté de nouveau) ne l'est pas deux fois.
    pub async fn record_export(&self, name: &str, seconds: f64, bytes: u64) -> Result<()> {
        let Some(table) = &self.table else { return Ok(()) };
        let failed = |f: Fail| anyhow::anyhow!(f.1);
        let acc = self.account().await.map_err(failed)?;
        let used = self.export_used_s().await.map_err(failed)?;
        let put = self.db.put_item().table_name(table)
            .item("pk", AttributeValue::S(format!("client#{}", self.client)))
            .item("sk", AttributeValue::S(format!("export#{}#{name}", month())))
            .item("secondes", AttributeValue::N(format!("{seconds:.1}")))
            .item("octets", AttributeValue::N(bytes.to_string()))
            .item("date", AttributeValue::S(iso(Utc::now())))
            .condition_expression("attribute_not_exists(sk)")
            .send().await;
        match put {
            Ok(_) => {}
            Err(e) if e.as_service_error().is_some_and(|s| s.is_conditional_check_failed_exception()) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let over = (used + seconds - self.plan_of(&acc).export_s()).clamp(0.0, seconds);
        if over > 0.0 && acc.credit_s > 0.0 {
            self.patch(&[("credit_s", Some(AttributeValue::N(format!("{:.1}", (acc.credit_s - over).max(0.0)))))]).await?;
        }
        Ok(())
    }

    /// Abonnement payé : le compte prend ce palier. Des rushs archivés entament leur récupération.
    /// `until` est l'échéance payée, quand c'est au service de la suivre.
    pub async fn subscribed(&self, key: &str, customer: Option<&str>, subscription: Option<&str>, until: Option<&str>) -> Result<()> {
        let acc = self.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        let s = |v: &str| Some(AttributeValue::S(v.into()));
        let now = iso(Utc::now());
        let mut changes = vec![("palier", s(key)), ("depuis", s(&now)), ("fin_abonnement", None), ("resiliation", None),
                               ("echeance", until.and_then(s)), ("paiement_echec", None)];
        if acc.archived.is_some() && acc.purged.is_none() {
            changes.push(("recuperation", s(&now)));
        }
        changes.extend([("archive", None), ("purge", None)]);
        for (attr, v) in [("paiement_client", customer), ("paiement_abonnement", subscription)] {
            if let Some(v) = v {
                changes.push((attr, s(v)));
            }
        }
        self.patch(&changes).await
    }

    /// Fin de l'abonnement : retour au palier d'essai, et début du délai d'accès aux rushs.
    pub async fn unsubscribed(&self) -> Result<()> {
        let s = |v: &str| Some(AttributeValue::S(v.into()));
        self.patch(&[("palier", s(FREE)), ("fin_abonnement", s(&iso(Utc::now()))), ("resiliation", None), ("paiement_abonnement", None),
                     ("echeance", None), ("paiement_echec", None)]).await
    }
}

pub async fn status(c: Scope) -> Result<Json<Value>, Fail> {
    let acc = c.account().await?;
    let (plan, standing, (used, frozen)) = (c.plan_of(&acc), c.standing_of(&acc), c.storage().await?);
    let recovery = matches!(standing, Standing::Archived { .. }).then(|| c.policy.recovery_eur(used));
    Ok(Json(json!({
        "plan": {"key": plan.key, "label": plan.label, "quota_bytes": plan.quota_bytes(), "export_s": plan.export_s()},
        "used_bytes": used, "export_used_s": c.export_used_s().await?,
        "credit_s": acc.credit_s, "credit": {"min": c.policy.credit_min, "eur": c.policy.credit_eur},
        "standing": standing.json(), "subscribed": acc.subscription.is_some(), "cancel_at": acc.cancel_at,
        "recovery_eur": recovery, "access_days": c.policy.access_days,
        "overage": {"credits_100go": c.policy.overage_credits_100go, "grace_days": c.policy.overage_grace_days,
                    "bytes": used.saturating_sub(plan.quota_bytes()), "out_since": acc.over_out,
                    "frozen_bytes": frozen, "recovery_credits": c.policy.recovery_s(frozen) / 60.0},
        "paid_until": acc.paid_until, "payment_failed": acc.failed, "grace_days": c.policy.grace_days,
        "plans": c.plans, "payment": c.payment.is_some(), "provider": c.payment.as_ref().map(|p| p.name()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy { trial_days: 7.0, access_days: 30.0, archive_days: 180.0, credit_min: 60, credit_eur: 0.07, recovery_eur_100go: 3.0, grace_days: 14.0,
                 overage_credits_100go: 25.0, overage_min_days: 7.0, overage_grace_days: 30.0 }
    }

    fn ago(days: i64) -> Option<String> {
        Some(iso(Utc::now() - chrono::Duration::days(days)))
    }

    #[test]
    fn a_trial_runs_from_the_first_upload_then_ends() {
        let (p, now) = (policy(), Utc::now());
        assert_eq!(Account::default().standing(false, &p, now), Standing::Trial { days_left: None });
        let started = Account { trial_start: ago(3), ..Default::default() };
        assert!(matches!(started.standing(false, &p, now), Standing::Trial { days_left: Some(d) } if (d - 4.0).abs() < 0.01));
        let over = Account { trial_start: ago(7), ..Default::default() };
        assert_eq!(over.standing(false, &p, now), Standing::TrialOver);
        assert_eq!(over.standing(true, &p, now), Standing::Paid, "un abonné n'est plus à l'essai");
    }

    #[test]
    fn after_a_subscription_access_then_archive_then_nothing() {
        let (p, now) = (policy(), Utc::now());
        let at = |days| Account { trial_start: ago(400), ended: ago(days), ..Default::default() };
        assert!(matches!(at(10).standing(false, &p, now), Standing::Ended { days_left } if (days_left - 20.0).abs() < 0.01));
        assert!(matches!(at(40).standing(false, &p, now), Standing::Archived { days_left } if (days_left - 170.0).abs() < 0.01));
        assert!(matches!(at(300).standing(false, &p, now), Standing::Archived { days_left } if days_left == 0.0));
        assert_eq!(Account { purged: ago(1), ..at(300) }.standing(false, &p, now), Standing::Purged);
        assert_eq!(Account { restoring: ago(0), ..at(40) }.standing(true, &p, now), Standing::Restoring);
    }

    #[test]
    fn recovery_is_priced_by_started_100_go() {
        let p = policy();
        assert_eq!(p.recovery_eur(1), 3.0);
        assert_eq!(p.recovery_eur(100_000_000_000), 3.0);
        assert_eq!(p.recovery_eur(600_000_000_001), 21.0);
        assert_eq!(p.recovery_s(100_000_000_000), 43.0 * 60.0, "3 € à 0,07 € le crédit : 43 crédits");
    }

    #[test]
    fn overage_costs_25_credits_per_100_go_per_30_days() {
        let p = policy();
        assert_eq!(p.overage_s(100_000_000_000, 30.0), 25.0 * 60.0);
        assert_eq!(p.overage_s(300_000_000_000, 30.0), 75.0 * 60.0);
        assert!((p.overage_s(100_000_000_000, 1.0) - 50.0).abs() < 1e-9);
        assert_eq!(p.overage_s(0, 30.0), 0.0);
    }
}
