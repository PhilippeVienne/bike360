//! Chapitres YouTube d'un montage : horodatages dans la vidéo finale + nom de lieu, prêts à
//! coller dans la description.
//!
//! Règles de YouTube : le premier chapitre commence à 0:00, au moins 3 chapitres, chacun
//! d'au moins 10 s. Les clips consécutifs au même lieu sont regroupés ; un chapitre trop
//! court est fondu dans le précédent. Le temps de sortie tient compte des transitions
//! (chevauchement de `transition_s` entre clips) et de la carte de fin.

use crate::numeric::round_nd;

pub const MIN_CHAPTER_S: f64 = 10.0;
pub const MIN_CHAPTERS: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start: f64,
    pub title: String,
}

/// Horodatage YouTube : M:SS, ou H:MM:SS au-delà d'une heure.
pub fn stamp(t: f64) -> String {
    let s = t.max(0.0).floor() as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Chapitres à partir des clips dans l'ordre du montage : (durée en s, lieu ; vide si inconnu).
/// `end_card_s` : durée de la carte de fin (0 sans), chapitre « Bilan de la balade ».
pub fn chapters(clips: &[(f64, String)], transition_s: f64, end_card_s: f64) -> Vec<Chapter> {
    let mut out: Vec<Chapter> = vec![];
    let mut t = 0.0;
    let n = clips.len() + usize::from(end_card_s > 0.0);
    // même règle que finishing : transition au plus la moitié du plus court élément
    let shortest = clips.iter().map(|c| c.0).chain((end_card_s > 0.0).then_some(end_card_s)).fold(f64::INFINITY, f64::min);
    let tr = if n > 1 { transition_s.min(shortest / 2.0) } else { 0.0 };
    let items = clips.iter().cloned().chain((end_card_s > 0.0).then(|| (end_card_s, "Bilan de la balade".to_string())));
    for (k, (dur, place)) in items.enumerate() {
        let title = if place.is_empty() { format!("Passage {}", k + 1) } else { place };
        if out.last().is_none_or(|c| c.title != title) {
            out.push(Chapter { start: round_nd(t, 1), title });
        }
        t += dur - tr;
    }
    let total = t + tr;
    // fond les chapitres trop courts dans le précédent (le premier reste à 0:00)
    let mut k = 1;
    while k < out.len() {
        let end = out.get(k + 1).map_or(total, |c| c.start);
        if end - out[k].start < MIN_CHAPTER_S {
            out.remove(k);
        } else if out[k].title == out[k - 1].title {
            out.remove(k);
        } else {
            k += 1;
        }
    }
    if let Some(first) = out.first_mut() {
        first.start = 0.0;
    }
    if out.len() > 1 && out[0].start + MIN_CHAPTER_S > out[1].start {
        let second = out.remove(1);
        out[0].title = second.title;   // ouverture trop courte : on garde le lieu suivant
    }
    out
}

/// Texte à coller dans la description ; vide si YouTube refuserait les chapitres (moins de 3).
pub fn description(chapters: &[Chapter]) -> String {
    if chapters.len() < MIN_CHAPTERS {
        return String::new();
    }
    chapters.iter().map(|c| format!("{} {}", stamp(c.start), c.title)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clips(v: &[(f64, &str)]) -> Vec<(f64, String)> {
        v.iter().map(|(d, p)| (*d, p.to_string())).collect()
    }

    #[test]
    fn regroupe_et_decale_les_transitions() {
        let c = chapters(&clips(&[(12.0, "Bourg-d'Oisans"), (14.0, "Bourg-d'Oisans"), (12.0, "Alpe d'Huez"), (13.0, "Besse")]), 0.6, 5.0);
        let got: Vec<(f64, &str)> = c.iter().map(|c| (c.start, c.title.as_str())).collect();
        // carte de fin (5 s) plus courte que le minimum de YouTube : fondue dans « Besse »
        assert_eq!(got, vec![(0.0, "Bourg-d'Oisans"), (24.8, "Alpe d'Huez"), (36.2, "Besse")]);
        assert_eq!(description(&c).lines().next(), Some("0:00 Bourg-d'Oisans"));
    }

    #[test]
    fn chapitres_courts_fondus() {
        let c = chapters(&clips(&[(20.0, "A"), (6.0, "B"), (20.0, "A"), (15.0, "C")]), 0.0, 0.0);
        let got: Vec<(f64, &str)> = c.iter().map(|c| (c.start, c.title.as_str())).collect();
        assert_eq!(got, vec![(0.0, "A"), (46.0, "C")]);
        assert_eq!(description(&c), "");   // moins de 3 chapitres : refusé par YouTube
    }

    #[test]
    fn horodatages() {
        assert_eq!(stamp(0.0), "0:00");
        assert_eq!(stamp(65.9), "1:05");
        assert_eq!(stamp(3725.0), "1:02:05");
    }
}
