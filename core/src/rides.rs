//! Balades : sessions qui se suivent de près, toutes caméras confondues. Les temps sont ceux
//! recalés sur le GPS (utc_t0 + décalage), donc comparables d'une caméra à l'autre.

/// Pause au-delà de laquelle une nouvelle balade commence.
pub const RIDE_GAP_S: f64 = 3.0 * 3600.0;
/// Recouvrement minimal pour que deux sessions soient deux angles du même moment.
pub const ANGLE_OVERLAP_S: f64 = 10.0;

/// Session vue par le regroupement : identifiant, début et fin (s UTC), numéro de série de la caméra.
pub struct Span<'a> {
    pub id: &'a str,
    pub start: f64,
    pub end: f64,
    pub camera: Option<&'a str>,
}

/// Pour chaque session (dans l'ordre reçu) : la balade (identifiant de sa première session) et les
/// autres sessions filmées au même moment par une autre caméra.
pub fn group<'a>(spans: &[Span<'a>]) -> Vec<(&'a str, Vec<&'a str>)> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by(|&a, &b| spans[a].start.total_cmp(&spans[b].start).then(spans[a].id.cmp(spans[b].id)));
    let mut ride = vec![""; spans.len()];
    let (mut first, mut end) = ("", f64::NEG_INFINITY);
    for &i in &order {
        if spans[i].start - end > RIDE_GAP_S {
            first = spans[i].id;
        }
        ride[i] = first;
        end = end.max(spans[i].end);
    }
    spans.iter().enumerate().map(|(i, s)| {
        let angles = spans.iter()
            .filter(|o| o.id != s.id && o.camera != s.camera && s.end.min(o.end) - s.start.max(o.start) >= ANGLE_OVERLAP_S)
            .map(|o| o.id)
            .collect();
        (ride[i], angles)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span<'a>(id: &'a str, start: f64, minutes: f64, camera: &'a str) -> Span<'a> {
        Span { id, start, end: start + minutes * 60.0, camera: Some(camera) }
    }

    #[test]
    fn rides_split_on_long_pauses() {
        let h = 3600.0;
        let spans = [span("c", 5.0 * h, 30.0, "X"), span("a", 0.0, 30.0, "X"), span("b", 1.0 * h, 30.0, "X"), span("d", 9.0 * h, 10.0, "X")];
        let rides: Vec<&str> = group(&spans).into_iter().map(|g| g.0).collect();
        // c commence 3 h 30 après la fin de b : nouvelle balade ; d suit c à 3 h 30 de sa fin aussi
        assert_eq!(rides, ["c", "a", "a", "d"]);
    }

    #[test]
    fn overlapping_cameras_are_angles() {
        let spans = [span("a", 0.0, 30.0, "X"), span("b", 600.0, 30.0, "Y"), span("c", 1795.0, 10.0, "Y"), span("d", 300.0, 5.0, "X")];
        let g = group(&spans);
        assert_eq!(g[0].1, ["b"], "c ne recouvre a que 5 s, d est la même caméra");
        assert_eq!(g[1].1, ["a"]);
        assert!(g[2].1.is_empty() && g[3].1.is_empty());
        assert!(g.iter().all(|x| x.0 == "a"));
    }
}
