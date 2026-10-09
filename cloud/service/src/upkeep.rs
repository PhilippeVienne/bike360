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
//! C'est aussi ici que se fait l'effacement complet d'un compte, à la demande de son titulaire.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, GlacierJobParameters, ObjectIdentifier, RestoreRequest, StorageClass,
                        Tag, Tagging, TaggingDirective, Tier};
use chrono::Utc;

use crate::account::Scope;
use crate::plans::{iso, Standing};
use crate::Ctx;

/// Étiquette des objets à passer en archive profonde.
const ARCHIVE_TAG: (&str, &str) = ("etat", "archive");
/// Jours pendant lesquels un objet rendu par l'archive reste lisible, le temps de le recopier.
const RESTORE_DAYS: i32 = 7;
/// Au-delà, S3 ne recopie un objet que par morceaux.
const COPY_MAX: u64 = 5 * 1024 * 1024 * 1024;
const COPY_PART: u64 = 1024 * 1024 * 1024;
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
        self.forget(&["rush#", "session#", "marque#"]).await?;
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

    /// Applique au compte ce que sa situation demande aujourd'hui.
    async fn upkeep(&self) -> Result<()> {
        let acc = self.account().await.map_err(|f| anyhow::anyhow!(f.1))?;
        let now = || Some(AttributeValue::S(iso(Utc::now())));
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

/// Passe sur les comptes à intervalle régulier, tant que le service tourne.
pub async fn watch(ctx: Arc<Ctx>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        if let Err(e) = sweep(&ctx).await {
            eprintln!("échéances : {e:#}");
        }
    }
}
