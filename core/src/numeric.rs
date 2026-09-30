//! Équivalents exacts des fonctions NumPy utilisées par l'analyse (mêmes conventions de bords).

/// NaN → 0 (np.nan_to_num).
pub fn nan_to_num(x: &[f64]) -> Vec<f64> {
    x.iter().map(|v| if v.is_finite() { *v } else { 0.0 }).collect()
}

/// Moyenne glissante centrée : np.convolve(nan_to_num(x), ones(n)/n, "same").
pub fn smooth(x: &[f64], n: usize) -> Vec<f64> {
    let n = n.min(x.len());
    if n <= 1 {
        return x.to_vec();
    }
    let x = nan_to_num(x);
    let m = x.len();
    let shift = (n - 1) / 2; // début de la partie « same » dans la convolution complète
    let w = 1.0 / n as f64;
    (0..m)
        .map(|i| {
            let k = i + shift; // indice dans la convolution complète (longueur m + n − 1)
            let lo = k.saturating_sub(n - 1);
            let hi = k.min(m - 1);
            (lo..=hi).map(|j| x[j]).sum::<f64>() * w
        })
        .collect()
}

/// Rang percentile 0..1 (tri stable, comme np.argsort(kind="stable") + linspace).
pub fn rank(x: &[f64]) -> Vec<f64> {
    let x = nan_to_num(x);
    let n = x.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| x[a].partial_cmp(&x[b]).unwrap()); // −0 == 0, comme NumPy
    let mut r = vec![0.0; n];
    for (k, &i) in idx.iter().enumerate() {
        r[i] = if n > 1 { k as f64 / (n - 1) as f64 } else { 0.0 };
    }
    r
}

/// Dérivée : différences centrées, décentrées d'ordre 1 aux bords (np.gradient).
pub fn gradient(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    if n < 2 {
        return vec![0.0; n];
    }
    (0..n)
        .map(|i| {
            if i == 0 {
                x[1] - x[0]
            } else if i == n - 1 {
                x[n - 1] - x[n - 2]
            } else {
                (x[i + 1] - x[i - 1]) / 2.0
            }
        })
        .collect()
}

/// Percentile avec interpolation linéaire (np.percentile par défaut).
pub fn percentile(x: &[f64], q: f64) -> f64 {
    let mut v: Vec<f64> = x.to_vec();
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let pos = q / 100.0 * (v.len() - 1) as f64;
    let (lo, frac) = (pos.floor() as usize, pos - pos.floor());
    if lo + 1 >= v.len() {
        v[lo]
    } else {
        v[lo] + (v[lo + 1] - v[lo]) * frac
    }
}

/// Médiane en ignorant les NaN (np.nanmedian).
pub fn nanmedian(x: &[f64]) -> f64 {
    let v: Vec<f64> = x.iter().copied().filter(|v| v.is_finite()).collect();
    percentile(&v, 50.0)
}

/// Premier indice i tel que t[i] ≥ v (np.searchsorted, côté gauche).
pub fn searchsorted(t: &[f64], v: f64) -> usize {
    t.partition_point(|x| *x < v)
}

/// Interpolation linéaire bornée aux extrémités (np.interp).
pub fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
    let n = xp.len();
    if n == 0 {
        return f64::NAN;
    }
    if x <= xp[0] {
        return fp[0];
    }
    if x >= xp[n - 1] {
        return fp[n - 1];
    }
    let j = searchsorted(xp, x).max(1);
    let (x0, x1) = (xp[j - 1], xp[j]);
    if x == x1 {
        return fp[j];
    }
    fp[j - 1] + (fp[j] - fp[j - 1]) * (x - x0) / (x1 - x0)
}

/// Déroulement d'angles en radians (np.unwrap).
pub fn unwrap(x: &[f64]) -> Vec<f64> {
    use std::f64::consts::PI;
    let mut out = Vec::with_capacity(x.len());
    let mut corr = 0.0;
    for (i, &v) in x.iter().enumerate() {
        if i > 0 {
            let d = v - x[i - 1];
            // np.unwrap : dd = mod(d + π, 2π) − π, avec −π conservé si d > 0
            let mut dd = (d + PI).rem_euclid(2.0 * PI) - PI;
            if dd == -PI && d > 0.0 {
                dd = PI;
            }
            if d.abs() >= PI {
                corr += dd - d;
            }
        }
        out.push(v + corr);
    }
    out
}

/// Coefficient de corrélation de Pearson (np.corrcoef[0, 1]).
pub fn corrcoef(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma) * (x - ma);
        sbb += (y - mb) * (y - mb);
    }
    sab / (saa * sbb).sqrt()
}

/// Arrondi décimal comme `round(x, nd)` en Python (au pair en cas d'égalité).
pub fn round_nd(x: f64, nd: i32) -> f64 {
    // même résultat que Python : arrondi du développement décimal exact du flottant
    if !x.is_finite() || nd < 0 {
        return x;
    }
    format!("{x:.*}", nd as usize).parse().unwrap_or(x)
}

/// Lissage gaussien à bords prolongés (np.pad « edge » + np.convolve « valid »).
pub fn gauss(x: &[f64], sig: f64) -> Vec<f64> {
    let r = (4.0 * sig) as i64;
    let w: Vec<f64> = (-r..=r).map(|k| (-((k * k) as f64) / (2.0 * sig * sig)).exp()).collect();
    let sum: f64 = w.iter().sum();
    let w: Vec<f64> = w.iter().map(|v| v / sum).collect();
    let n = x.len() as i64;
    if n == 0 {
        return vec![];
    }
    (0..n)
        .map(|i| (-r..=r).map(|k| x[(i + k).clamp(0, n - 1) as usize] * w[(k + r) as usize]).sum())
        .collect()
}
