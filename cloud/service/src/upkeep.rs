//! Échéances des comptes : un passage régulier applique ce que la situation de chaque compte
//! demande (voir `plans::Standing`).
//!
//!   essai terminé                    → ses rushs sont supprimés
//!   abonnement terminé, accès échu   → ses rushs sont étiquetés `etat=archive` ; une règle du
//!                                      compartiment les fait alors passer en archive profonde
//!   garde en archive échue           → ses rushs sont supprimés
//!   abonnement repris sur une archive → ses aperçus sont redemandés à l'archive puis remis dans
//!                                      leur classe ; les originaux y restent, comme tout original ancien
//!
//! C'est aussi ici que se fait l'effacement complet d'un compte, à la demande de son titulaire, et
//! la sortie d'archive des originaux qu'un export demande : ils y partent à 90 jours, l'export les
//! redemande, et le passage prévient le client quand ils sont revenus (`sk = sortie#<session>`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, GlacierJobParameters, ObjectIdentifier, RestoreRequest, StorageClass,
                        Tag, Tagging, TaggingDirective, Tier};
use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use chrono::Utc;

use crate::account::Scope;
use crate::plans::{days_between, iso, Standing};
use crate::Ctx;

/// Étiquette des objets à passer en archive profonde.
const ARCHIVE_TAG: (&str, &str) = ("etat", "archive");
/// Jours pendant lesquels un objet rendu par l'archive reste lisible, le temps de le recopier.
const RESTORE_DAYS: i32 = 7;
/// Au-delà, S3 ne recopie un objet que par morceaux.
const COPY_MAX: u64 = 5 * 1024 * 1024 * 1024;
const COPY_PART: u64 = 1024 * 1024 * 1024;
/// Réglages de la sortie d'archive des originaux demandés à l'export.
pub struct Thaw {
    /// Sortie « standard » (12 h pour l'archive profonde) au lieu de la sortie en masse (48 h), moins chère.
    pub standard: bool,
    /// Jours pendant lesquels les originaux revenus restent lisibles.
    pub days: i32,
    /// Messagerie et expéditeur des courriels qui préviennent le client.
    pub mail: Option<(aws_sdk_sesv2::Client, String)>,
}

impl Thaw {
    /// Délai annoncé au client, en heures.
    pub fn hours(&self) -> u32 {
        if self.standard { 12 } else { 48 }
    }
}

/// Préfixes du compartiment où un client a des objets.
const KINDS: [&str; 4] = ["apercus", "originaux", "exports", "donnees"];

impl Ctx {
    /// Objets sous `prefix` : (clé, taille, classe de stockage).
    async fn objects(&self, prefix: &str) -> Result<Vec<(String, u64, Option<StorageClass>)>> {
        let (mut out, mut token) = (vec![], None::<String>);
        loop {
            let page = self.s3.list_objects_v2().bucket(&self.bucket).prefix(prefix).set_continuation_token(token.take()).send().await?;
            for o in page.contents() {
                if let Some(key) = o.key() {
                    out.push((key.to_string(), o.size().unwrap_or(0) as u64, o.storage_class().map(|c| StorageClass::from(c.as_str()))));
                }
            }
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_string()),
                None => return Ok(out),
            }
        }
    }

    /// Supprime pour de bon tout ce qui est sous `prefix` : chaque version de chaque objet, et les envois en cours.
    async fn wipe(&self, prefix: &str) -> Result<usize> {
        let mut count = 0;
        loop {
            let page = self.s3.list_object_versions().bucket(&self.bucket).prefix(prefix).send().await?;
            let ids: Vec<ObjectIdentifier> = page.versions().iter().map(|v| (v.key(), v.version_id()))
                .chain(page.delete_markers().iter().map(|m| (m.key(), m.version_id())))
                .filter_map(|(key, version)| {
                    // un compartiment sans versions annonce la version « null », qui ne se nomme pas
                    ObjectIdentifier::builder().key(key?).set_version_id(version.filter(|v| *v != "null").map(String::from)).build().ok()
                })
                .collect();
            if ids.is_empty() {
                break;
            }
            count += ids.len();
            let out = self.s3.delete_objects().bucket(&self.bucket).delete(Delete::builder().set_objects(Some(ids)).quiet(true).build()?).send().await?;
            anyhow::ensure!(out.errors().is_empty(), "{} objet(s) non supprimé(s) sous {prefix}", out.errors().len());
        }
        let open = self.s3.list_multipart_uploads().bucket(&self.bucket).prefix(prefix).send().await?;
        for u in open.uploads() {
            if let (Some(key), Some(id)) = (u.key(), u.upload_id()) {
                self.s3.abort_multipart_upload().bucket(&self.bucket).key(key).upload_id(id).send().await?;
            }
        }
        Ok(count)
    }

    /// Vrai si l'objet, rangé dans une classe d'archive, n'est pas (encore) lisible ; `ask` demande alors
    /// sa sortie d'archive si elle ne l'est pas déjà.
    async fn frozen(&self, key: &str, class: &Option<StorageClass>, ask: bool) -> Result<bool> {
        if !matches!(class, Some(StorageClass::DeepArchive | StorageClass::Glacier)) {
            return Ok(false);
        }
        let head = self.s3.head_object().bucket(&self.bucket).key(key).send().await?;
        match head.restore() {
            Some(state) if state.contains("ongoing-request=\"false\"") => return Ok(false),
            Some(_) => {}
            None if ask => {
                let tier = if self.thaw.standard { Tier::Standard } else { Tier::Bulk };
                let request = RestoreRequest::builder().days(self.thaw.days).glacier_job_parameters(GlacierJobParameters::builder().tier(tier).build()?).build();
                if let Err(e) = self.s3.restore_object().bucket(&self.bucket).key(key).restore_request(request).send().await {
                    // une sortie déjà en cours n'est pas une erreur
                    anyhow::ensure!(format!("{e:?}").contains("RestoreAlreadyInProgress"), "sortie d'archive de {key} : {e:?}");
                }
            }
            None => {}
        }
        Ok(true)
    }

    async fn is_archived(&self, key: &str) -> Result<bool> {
        let tags = self.s3.get_object_tagging().bucket(&self.bucket).key(key).send().await?;
        Ok(tags.tag_set().iter().any(|t| (t.key(), t.value()) == ARCHIVE_TAG))
    }

    /// Recopie un objet sur lui-même dans une autre classe de stockage, sans son étiquette d'archive.
    async fn recopy(&self, key: &str, size: u64, class: StorageClass) -> Result<()> {
        let source = format!("{}/{key}", self.bucket);
        if size <= COPY_MAX {
            self.s3.copy_object().bucket(&self.bucket).key(key).copy_source(&source).storage_class(class)
                .tagging_directive(TaggingDirective::Replace).send().await?;
            return Ok(());
        }
        let upload = self.s3.create_multipart_upload().bucket(&self.bucket).key(key).storage_class(class).send().await?;
        let id = upload.upload_id().context("recopie sans identifiant")?;
        let mut parts = vec![];
        for (n, start) in (0..size).step_by(COPY_PART as usize).enumerate() {
            let end = (start + COPY_PART).min(size) - 1;
            let part = self.s3.upload_part_copy().bucket(&self.bucket).key(key).upload_id(id).part_number(n as i32 + 1)
                .copy_source(&source).copy_source_range(format!("bytes={start}-{end}")).send().await?;
            let tag = part.copy_part_result().and_then(|r| r.e_tag()).context("morceau recopié sans étiquette")?;
            parts.push(CompletedPart::builder().part_number(n as i32 + 1).e_tag(tag).build());
        }
        self.s3.complete_multipart_upload().bucket(&self.bucket).key(key).upload_id(id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await?;
        Ok(())
    }
}

impl Scope {
    /// Retire de l'index les lignes du client dont la clé de tri commence par l'un de ces préfixes.
    async fn forget(&self, prefixes: &[&str]) -> Result<()> {
        let table = self.table.as_ref().context("index non configuré")?;
        for prefix in prefixes {
            for item in self.rows(prefix).await.map_err(|f| anyhow::anyhow!(f.1))? {
                let (Some(pk), Some(sk)) = (item.get("pk"), item.get("sk")) else { continue };
                self.db.delete_item().table_name(table).key("pk", pk.clone()).key("sk", sk.clone()).send().await?;
            }
        }
        Ok(())
    }

    /// Supprime les rushs du client et tout ce qui en découle ; son compte et sa consommation restent.
    async fn drop_rushs(&self) -> Result<usize> {
        self.atelier_stop(false).await?;
        let mut count = 0;
        for kind in KINDS {
            count += self.wipe(&format!("{kind}/{}/", self.client)).await?;
        }
        self.forget(&["rush#", "session#", "marque#", "sortie#"]).await?;
        Ok(count)
    }

    /// Efface tout ce que le service garde du client : fichiers, index, référence de paiement.
    pub async fn erase(&self, customer: Option<&str>) -> Result<usize> {
        let count = self.drop_rushs().await?;
        self.forget(&[""]).await?;
        if let (Some(table), Some(customer)) = (&self.table, customer) {
            self.db.delete_item().table_name(table).key("pk", AttributeValue::S(format!("paiement#{customer}")))
                .key("sk", AttributeValue::S("client".into())).send().await?;
        }
        Ok(count)
    }

    /// Étiquette les rushs du client pour l'archive profonde ; son atelier est enregistré puis arrêté.
    async fn archive(&self) -> Result<usize> {
        self.atelier_stop(true).await?;
        let tag = Tag::builder().key(ARCHIVE_TAG.0).value(ARCHIVE_TAG.1).build()?;
        let mut count = 0;
        for kind in ["apercus", "originaux"] {
            for (key, _, _) in self.objects(&format!("{kind}/{}/", self.client)).await? {
                self.s3.put_object_tagging().bucket(&self.bucket).key(&key)
                    .tagging(Tagging::builder().tag_set(tag.clone()).build()?).send().await?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Fait avancer la récupération d'une archive ; renvoie le nombre d'aperçus encore attendus.
    async fn restore(&self) -> Result<usize> {
        let mut waiting = 0;
        for kind in ["apercus", "originaux"] {
            for (key, size, class) in self.objects(&format!("{kind}/{}/", self.client)).await? {
                if !self.is_archived(&key).await? {
                    continue;
                }
                let cold = matches!(class, Some(StorageClass::DeepArchive | StorageClass::Glacier));
                // un objet que la règle d'archivage n'a pas encore déplacé n'a qu'à perdre son étiquette ;
                // un original archivé le reste, comme tous ceux qui ont l'âge de l'être
                if !cold || kind == "originaux" {
                    self.s3.delete_object_tagging().bucket(&self.bucket).key(&key).send().await?;
                    continue;
                }
                let head = self.s3.head_object().bucket(&self.bucket).key(&key).send().await?;
                match head.restore() {
                    Some(state) if state.contains("ongoing-request=\"false\"") => self.recopy(&key, size, StorageClass::IntelligentTiering).await?,
                    Some(_) => waiting += 1,
                    None => {
                        let request = RestoreRequest::builder().days(RESTORE_DAYS)
                            .glacier_job_parameters(GlacierJobParameters::builder().tier(Tier::Bulk).build()?).build();
                        self.s3.restore_object().bucket(&self.bucket).key(&key).restore_request(request).send().await?;
                        waiting += 1;
                    }
                }
            }
        }
        Ok(waiting)
    }

    /// Originaux d'une session (`VID_<date>_<heure>`) encore retenus par l'archive.
    async fn frozen_originals(&self, session: &str, ask: bool) -> Result<usize> {
        let prefix = format!("originaux/{}/{session}_", self.client);
        let mut waiting = 0;
        for (key, _, class) in self.objects(&prefix).await? {
            if self.frozen(&key, &class, ask).await? {
                waiting += 1;
            }
        }
        Ok(waiting)
    }

    /// Demande à l'archive les originaux de ces sessions (préfixes `VID_<date>_<heure>_`) qui y sont ;
    /// renvoie le nombre de fichiers attendus. Chaque session en attente est notée dans l'index.
    pub async fn thaw(&self, sessions: &[String]) -> Result<usize> {
        let mut waiting = 0;
        for session in sessions.iter().map(|s| s.trim_end_matches('_')) {
            let n = self.frozen_originals(session, true).await?;
            waiting += n;
            let Some(table) = self.table.as_ref().filter(|_| n > 0) else { continue };
            // la première demande date l'attente ; les suivantes ne la remettent pas à zéro
            let put = self.db.put_item().table_name(table)
                .item("pk", AttributeValue::S(format!("client#{}", self.client))).item("sk", AttributeValue::S(format!("sortie#{session}")))
                .item("etat", AttributeValue::S("demandee".into())).item("demande", AttributeValue::S(iso(Utc::now())))
                .condition_expression("attribute_not_exists(sk) OR etat <> :d").expression_attribute_values(":d", AttributeValue::S("demandee".into()))
                .send().await;
            if let Err(e) = put {
                anyhow::ensure!(e.as_service_error().is_some_and(|s| s.is_conditional_check_failed_exception()), "suivi de la sortie d'archive : {e:?}");
            }
        }
        Ok(waiting)
    }

    /// Prévient le client par courriel, si la messagerie est configurée et son adresse connue.
    pub(crate) async fn mail(&self, subject: &str, text: &str) -> Result<bool> {
        let (Some((ses, from)), Some(auth)) = (&self.thaw.mail, &self.auth) else { return Ok(false) };
        let to = auth.email_of(&self.client).await?;
        let part = |v: &str| Content::builder().data(v).charset("UTF-8").build();
        let message = Message::builder().subject(part(subject)?).body(Body::builder().text(part(text)?).build()).build();
        ses.send_email().from_email_address(from).destination(Destination::builder().to_addresses(to).build())
            .content(EmailContent::builder().simple(message).build()).send().await?;
        Ok(true)
    }

    /// Suit les sorties d'archive demandées : prévient le client quand ses originaux sont revenus,
    /// puis oublie la demande quand ils sont repartis.
    async fn follow_thaws(&self) -> Result<()> {
        let Some(table) = &self.table else { return Ok(()) };
        for item in self.rows("sortie#").await.map_err(|f| anyhow::anyhow!(f.1))? {
            let text = |k: &str| item.get(k).and_then(|v| v.as_s().ok()).cloned();
            let Some(sk) = text("sk") else { continue };
            let session = sk.trim_start_matches("sortie#");
            let row = self.db.update_item().table_name(table).key("pk", AttributeValue::S(format!("client#{}", self.client))).key("sk", AttributeValue::S(sk.clone()));
            if text("etat").as_deref() == Some("prete") {
                let gone = text("jusque").and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).is_none_or(|t| t < Utc::now());
                if gone {
                    self.db.delete_item().table_name(table).key("pk", AttributeValue::S(format!("client#{}", self.client))).key("sk", AttributeValue::S(sk.clone())).send().await?;
                }
                continue;
            }
            if self.frozen_originals(session, true).await? > 0 {
                continue;
            }
            let until = Utc::now() + chrono::Duration::days(self.thaw.days as i64);
            row.update_expression("SET etat = :e, prete = :p, jusque = :j")
                .expression_attribute_values(":e", AttributeValue::S("prete".into()))
                .expression_attribute_values(":p", AttributeValue::S(iso(Utc::now())))
                .expression_attribute_values(":j", AttributeValue::S(iso(until))).send().await?;
            let when = format!("{}/{}/{} à {}h{}", &session[10..12], &session[8..10], &session[4..8], &session[13..15], &session[15..17]);
            let sent = self.mail("Bike360 : tes originaux sont prêts pour l'export", &format!(
                "Bonjour,\n\nLes originaux de ta session du {when} sont sortis de l'archive. Ils restent disponibles {} jours : \
                 ouvre l'atelier et relance ton export.\n\n{}/ui/bibliotheque.html\n", self.thaw.days, self.site)).await;
            match sent {
                Ok(sent) => println!("originaux de {session} revenus de l'archive pour {} ({})", self.client, if sent { "courriel envoyé" } else { "sans courriel" }),
                Err(e) => eprintln!("originaux de {session} revenus, courriel non envoyé à {} : {e:#}", self.client),
            }
        }
        Ok(())
    }

    /// Dépassement du quota de stockage d'un abonné : le temps passé au-dessus est pris sur son crédit.
    /// Crédit épuisé, il a `overage_grace_days` jours pour régulariser ; ensuite ses rushs les plus anciens
    /// partent en archive jusqu'à ce que le reste tienne dans le quota. Un rush ainsi archivé (`gele`) ne
    /// compte plus dans le quota, se récupère en crédits, et est supprimé au terme de sa garde.
    async fn meter_overage(&self, acc: &crate::plans::Account) -> Result<()> {
        let Some(table) = &self.table else { return Ok(()) };
        let quota = self.plan_of(acc).quota_bytes();
        let now = Utc::now();
        let text = |i: &std::collections::HashMap<String, AttributeValue>, k: &str| i.get(k).and_then(|v| v.as_s().ok()).cloned();
        let bytes = |i: &std::collections::HashMap<String, AttributeValue>| i.get("octets").and_then(|v| v.as_n().ok()).and_then(|n| n.parse::<f64>().ok()).unwrap_or(0.0) as u64;
        let mut live = vec![];
        for item in self.rows("rush#").await.map_err(|f| anyhow::anyhow!(f.1))? {
            let (Some(sk), Some(key)) = (text(&item, "sk"), text(&item, "cle")) else { continue };
            match text(&item, "gele") {
                // garde en archive échue : le rush est supprimé pour de bon
                Some(since) if days_between(&since, now) >= self.policy.archive_days => {
                    self.wipe(&key).await?;
                    self.db.delete_item().table_name(table).key("pk", AttributeValue::S(format!("client#{}", self.client)))
                        .key("sk", AttributeValue::S(sk)).send().await?;
                }
                Some(_) => {}
                None => live.push((text(&item, "recu").unwrap_or_default(), sk, key, bytes(&item))),
            }
        }
        let over = live.iter().map(|r| r.3).sum::<u64>().saturating_sub(quota);
        if over == 0 {
            if acc.over_seen.is_some() || acc.over_out.is_some() {
                self.patch(&[("depassement_vu", None), ("depassement_fin", None)]).await?;
            }
            return Ok(());
        }
        let stamp = Some(AttributeValue::S(iso(now)));
        let credit = match &acc.over_seen {
            Some(seen) => (acc.credit_s - self.policy.overage_s(over, days_between(seen, now).max(0.0))).max(0.0),
            None => acc.credit_s,
        };
        let mut changes = vec![("depassement_vu", stamp.clone()), ("credit_s", Some(AttributeValue::N(format!("{credit:.1}"))))];
        if credit > 0.0 {
            changes.push(("depassement_fin", None));
            return self.patch(&changes).await;
        }
        match &acc.over_out {
            None => {
                changes.push(("depassement_fin", stamp));
                self.patch(&changes).await?;
                let sent = self.mail("Bike360 : ton crédit est épuisé et ton quota dépassé", &format!(
                    "Bonjour,\n\nTon stockage dépasse ton quota de {:.0} Go et ton crédit est épuisé. Tu as {:.0} jours pour libérer de la place, \
                     racheter du crédit ou changer de palier ; ensuite tes rushs les plus anciens partiront en archive.\n\n{}/ui/palier.html\n",
                    over as f64 / 1e9, self.policy.overage_grace_days, self.site)).await;
                if let Err(e) = sent {
                    eprintln!("dépassement de {} : courriel non envoyé : {e:#}", self.client);
                }
                println!("dépassement de {} : crédit épuisé", self.client);
            }
            Some(out) if days_between(out, now) >= self.policy.overage_grace_days => {
                live.sort();   // les plus anciens d'abord (date de réception)
                let tag = Tag::builder().key(ARCHIVE_TAG.0).value(ARCHIVE_TAG.1).build()?;
                let (mut freed, mut count) = (0, 0);
                for (_, sk, key, size) in &live {
                    if freed >= over {
                        break;
                    }
                    self.s3.put_object_tagging().bucket(&self.bucket).key(key).tagging(Tagging::builder().tag_set(tag.clone()).build()?).send().await?;
                    self.db.update_item().table_name(table).key("pk", AttributeValue::S(format!("client#{}", self.client)))
                        .key("sk", AttributeValue::S(sk.clone())).update_expression("SET gele = :d")
                        .expression_attribute_values(":d", AttributeValue::S(iso(now))).send().await?;
                    freed += size;
                    count += 1;
                }
                self.patch(&[("depassement_vu", None), ("depassement_fin", None)]).await?;
                println!("dépassement de {} non régularisé : {count} rush(s) envoyé(s) en archive", self.client);
            }
            Some(_) => self.patch(&changes).await?,
        }
        Ok(())
    }

    /// Applique au compte ce que sa situation demande aujourd'hui.
    async fn upkeep(&self) -> Result<()> {
        self.follow_thaws().await?;
        let mut acc = self.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        // un abonnement dont le prestataire n'annonce pas la fin s'arrête ici, à son échéance
        if crate::payment::lapse(self, &acc).await? {
            acc = self.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        }
        let now = || Some(AttributeValue::S(iso(Utc::now())));
        if self.standing_of(&acc) == Standing::Paid {
            self.meter_overage(&acc).await?;
        }
        match self.standing_of(&acc) {
            Standing::TrialOver if acc.purged.is_none() => {
                let n = self.drop_rushs().await?;
                self.patch(&[("purge", now())]).await?;
                println!("essai de {} terminé : {n} objet(s) supprimé(s)", self.client);
            }
            Standing::Archived { days_left } if days_left <= 0.0 => {
                let n = self.drop_rushs().await?;
                self.patch(&[("purge", now())]).await?;
                println!("archive de {} échue : {n} objet(s) supprimé(s)", self.client);
            }
            Standing::Archived { .. } if acc.archived.is_none() => {
                let n = self.archive().await?;
                self.patch(&[("archive", now())]).await?;
                println!("rushs de {} envoyés en archive : {n} objet(s)", self.client);
            }
            Standing::Restoring => {
                if self.restore().await? == 0 {
                    self.patch(&[("recuperation", None)]).await?;
                    println!("rushs de {} revenus de l'archive", self.client);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Récupère les rushs partis en archive pour dépassement, contre des crédits : il faut que tout tienne
/// de nouveau dans le quota, ou que le crédit restant couvre le dépassement quelques jours.
pub async fn recover(c: Scope) -> Result<axum::Json<serde_json::Value>, crate::Fail> {
    use axum::http::StatusCode;
    let refuse = |code, text: String| crate::Fail(code, text);
    let failed = |e: anyhow::Error| {
        eprintln!("récupération : {e:#}");
        crate::Fail(StatusCode::BAD_GATEWAY, "récupération impossible pour l'instant".into())
    };
    let table = c.table.clone().ok_or_else(|| refuse(StatusCode::SERVICE_UNAVAILABLE, "index non configuré".into()))?;
    let acc = c.account().await?;
    if c.standing_of(&acc) != Standing::Paid {
        return Err(refuse(StatusCode::PAYMENT_REQUIRED, "la récupération demande un abonnement en cours".into()));
    }
    let (live, frozen) = c.storage().await?;
    if frozen == 0 {
        return Err(refuse(StatusCode::CONFLICT, "aucun rush en archive à récupérer".into()));
    }
    let cost = c.policy.recovery_s(frozen);
    let over = (live + frozen).saturating_sub(c.plan_of(&acc).quota_bytes());
    let need = cost + c.policy.overage_s(over, c.policy.overage_min_days);
    if acc.credit_s < need {
        return Err(refuse(StatusCode::PAYMENT_REQUIRED, format!(
            "il faut {:.0} crédit(s) et tu en as {:.0} : {:.0} pour la récupération{}",
            (need / 60.0).ceil(), (acc.credit_s / 60.0).floor(), cost / 60.0,
            if over > 0 { format!(", le reste pour {:.0} Go au-dessus du quota", over as f64 / 1e9) } else { String::new() })));
    }
    for item in c.rows("rush#").await? {
        let (Some(pk), Some(sk)) = (item.get("pk"), item.get("sk")) else { continue };
        if item.contains_key("gele") {
            c.db.update_item().table_name(&table).key("pk", pk.clone()).key("sk", sk.clone()).update_expression("REMOVE gele")
                .send().await.map_err(|e| failed(e.into()))?;
        }
    }
    // la sortie d'archive elle-même est faite par le passage des échéances, comme après une reprise d'abonnement
    c.patch(&[("credit_s", Some(AttributeValue::N(format!("{:.1}", acc.credit_s - cost)))), ("recuperation", Some(AttributeValue::S(iso(Utc::now()))))])
        .await.map_err(failed)?;
    Ok(axum::Json(serde_json::json!({"ok": true, "bytes": frozen, "credits": cost / 60.0})))
}

/// Un passage sur tous les comptes.
async fn sweep(ctx: &Arc<Ctx>) -> Result<()> {
    let Some(table) = &ctx.table else { return Ok(()) };
    let mut from = None;
    loop {
        let page = ctx.db.scan().table_name(table).filter_expression("sk = :c")
            .expression_attribute_values(":c", AttributeValue::S("compte".into()))
            .projection_expression("pk").set_exclusive_start_key(from.take()).send().await?;
        for item in page.items() {
            let Some(client) = item.get("pk").and_then(|v| v.as_s().ok()).and_then(|pk| pk.strip_prefix("client#")) else { continue };
            // l'échec d'un compte n'arrête pas le passage : il sera repris au suivant
            if let Err(e) = Scope::of(ctx.clone(), client.to_string()).upkeep().await {
                eprintln!("échéances de {client} : {e:#}");
            }
        }
        match page.last_evaluated_key() {
            Some(key) => from = Some(key.clone()),
            None => return Ok(()),
        }
    }
}

/// Un passage à la demande (`--sweep-route`) : sur AWS, une règle planifiée l'appelle à la place de la boucle.
pub async fn once(axum::extract::State(ctx): axum::extract::State<Arc<Ctx>>) -> Result<axum::Json<serde_json::Value>, crate::Fail> {
    sweep(&ctx).await.map_err(|e| {
        eprintln!("échéances : {e:#}");
        crate::Fail(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "passage interrompu".into())
    })?;
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

/// Passe sur les comptes à intervalle régulier, tant que le service tourne.
pub async fn watch(ctx: Arc<Ctx>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        if let Err(e) = sweep(&ctx).await {
            eprintln!("échéances : {e:#}");
        }
    }
}
