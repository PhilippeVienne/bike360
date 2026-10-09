//! Service hébergé : les originaux d'une session ne sont amenés sur la machine de l'atelier qu'au
//! moment d'exporter. L'atelier les demande au service qui l'a lancé ($BIKE360_ORIGINALS_URL), en
//! s'identifiant par son mot de passe, que ce service est seul à connaître.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Les originaux pèsent des dizaines de Go : la copie peut durer.
const TIMEOUT: Duration = Duration::from_secs(3600);

/// Adresse du service à qui demander les originaux, si l'atelier est hébergé.
pub fn service_url() -> Option<String> {
    std::env::var("BIKE360_ORIGINALS_URL").ok().filter(|u| !u.is_empty())
}

/// Demande les originaux de ces sessions ; revient quand ils sont dans le dossier de rushs.
pub fn request(url: &str, sessions: &[String]) -> Result<()> {
    let token = std::env::var("BIKE360_PASSWORD").unwrap_or_default();
    match ureq::post(url).set("Authorization", &format!("Bearer {token}")).timeout(TIMEOUT).send_json(json!({"sessions": sessions})) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(_, res)) => {
            let reason = res.into_json::<Value>().ok().and_then(|v| v["error"].as_str().map(String::from));
            bail!("{}", reason.unwrap_or_else(|| "originaux indisponibles".into()))
        }
        Err(e) => bail!("service des originaux injoignable : {e}"),
    }
}
