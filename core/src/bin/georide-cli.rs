//! Remplaçant Rust de `georide.py` : même interface en ligne de commande.
//!
//!     georide-cli trackers
//!     georide-cli positions 2026-08-29 2026-08-30 [--tracker ID] [-o fichier.json]
//!     georide-cli trips     2026-08-29 2026-08-30 [--tracker ID] [-o fichier.json]
//!
//! Écrit comme un petit pipeline de closures (chacune : args -> Result<Value>) plutôt
//! qu'un match géant, pour qu'une future exécution « une closure = un handler Lambda »
//! n'ait presque rien à changer à cette logique métier.
use std::path::Path;

use anyhow::{bail, Context, Result};
use bike360_core::georide;
use serde_json::Value;

/// Options communes aux sous-commandes `positions`/`trips` : date début, date fin,
/// tracker explicite, fichier de sortie.
struct RangeArgs<'a> {
    start: &'a str,
    end: &'a str,
    tracker: Option<&'a str>,
    output: Option<&'a str>,
}

fn parse_range_args(args: &[String]) -> Result<RangeArgs<'_>> {
    let mut positional = vec![];
    let mut tracker = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tracker" => {
                tracker = Some(args.get(i + 1).context("--tracker attend une valeur")?.as_str());
                i += 2;
            }
            "-o" | "--output" => {
                output = Some(args.get(i + 1).context("-o attend un fichier")?.as_str());
                i += 2;
            }
            other => {
                positional.push(other);
                i += 1;
            }
        }
    }
    if positional.len() != 2 {
        bail!("usage: positions|trips DÉBUT FIN [--tracker ID] [-o fichier.json]");
    }
    Ok(RangeArgs { start: positional[0], end: positional[1], tracker, output })
}

/// `trackers` : une closure, aucun argument — liste id/nom/modèle, une ligne par tracker.
fn cmd_trackers(_args: &[String]) -> Result<Value> {
    georide::list_trackers()
}

/// `positions` : closure qui délègue au client GeoRide avec le tracker éventuellement forcé.
fn cmd_positions(args: &[String]) -> Result<Value> {
    let a = parse_range_args(args)?;
    georide::fetch_positions_for(a.start, a.end, a.tracker)
}

/// `trips` : même forme que `positions`, autre route GeoRide.
fn cmd_trips(args: &[String]) -> Result<Value> {
    let a = parse_range_args(args)?;
    georide::fetch_trips_for(a.start, a.end, a.tracker)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        bail!("usage: georide-cli trackers|positions|trips ...");
    };

    // Table de dispatch : nom -> closure (args -> Result<Value>). C'est ce bloc qui,
    // transposé tel quel, donnerait les handlers d'une fonction Lambda par commande.
    let handlers: &[(&str, fn(&[String]) -> Result<Value>)] =
        &[("trackers", cmd_trackers), ("positions", cmd_positions), ("trips", cmd_trips)];

    let Some((_, handler)) = handlers.iter().find(|(name, _)| *name == cmd.as_str()) else {
        bail!("commande inconnue : {cmd} (trackers|positions|trips)");
    };
    let data = handler(rest)?;

    if cmd == "trackers" {
        for t in data.as_array().context("liste de trackers attendue")? {
            println!(
                "{} {} {}",
                t.get("trackerId").map(Value::to_string).unwrap_or_default(),
                t.get("trackerName").and_then(Value::as_str).unwrap_or(""),
                t.get("model").and_then(Value::as_str).unwrap_or(""),
            );
        }
        return Ok(());
    }

    let a = parse_range_args(rest)?;
    let out = serde_json::to_string_pretty(&data)?;
    match a.output {
        Some(path) => {
            let n = data.as_array().map(Vec::len).unwrap_or(0);
            std::fs::write(Path::new(path), &out)?;
            eprintln!("{n} enregistrements → {path}");
        }
        None => println!("{out}"),
    }
    Ok(())
}
