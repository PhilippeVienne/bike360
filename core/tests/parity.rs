//! Non-régression : résultats identiques aux modules Python d'origine sur des données
//! synthétiques (références : tests/gen_fixtures.py, dans tests/fixtures/).

use std::collections::{BTreeMap, HashMap};

use insta_core::analyze::{self, Analysis, Gps};
use insta_core::geometry::{self, Clip, Tilt};
use insta_core::{automontage, horizon, hyperlapse, numeric::*};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn vec_f(v: &Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect()
}

#[track_caller]
fn close(a: &[f64], b: &[f64], tol: f64, what: &str) {
    assert_eq!(a.len(), b.len(), "{what} : longueurs");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let ok = (x.is_nan() && y.is_nan()) || (x - y).abs() <= tol * 1f64.max(y.abs());
        assert!(ok, "{what}[{i}] : {x} au lieu de {y}");
    }
}

#[test]
fn numeric() {
    let f = fixture("numeric");
    let (x, x_nan) = (vec_f(&f["x"]), vec_f(&f["x_nan"]));
    close(&smooth(&x_nan, 5), &vec_f(&f["smooth5"]), 1e-12, "smooth5");
    close(&smooth(&x, 30), &vec_f(&f["smooth30"]), 1e-12, "smooth30");
    let rounded: Vec<f64> = x_nan.iter().map(|v| v.round_ties_even()).collect();
    close(&rank(&rounded), &vec_f(&f["rank"]), 1e-12, "rank");
    close(&gradient(&x), &vec_f(&f["gradient"]), 1e-12, "gradient");
    close(&[percentile(&x, 15.0), percentile(&x, 99.5), nanmedian(&x_nan)],
          &[f["p15"].as_f64().unwrap(), f["p99_5"].as_f64().unwrap(), f["nanmedian"].as_f64().unwrap()], 1e-12, "percentiles");
    let xp: Vec<f64> = (0..x.len()).map(|k| k as f64).collect();
    let got: Vec<f64> = vec_f(&f["interp_at"]).iter().map(|t| interp(*t, &xp, &x)).collect();
    close(&got, &vec_f(&f["interp"]), 1e-12, "interp");
    close(&unwrap(&vec_f(&f["ang"])), &vec_f(&f["unwrap"]), 1e-12, "unwrap");
    close(&[corrcoef(&x[..x.len() - 1], &x[1..])], &[f["corr"].as_f64().unwrap()], 1e-12, "corrcoef");
    close(&gauss(&x, 15.0), &vec_f(&f["gauss15"]), 1e-12, "gauss15");
    close(&gauss(&x, 1.5), &vec_f(&f["gauss1_5"]), 1e-12, "gauss1_5");
    for r in f["round"].as_array().unwrap() {
        let (v, nd, want) = (r[0].as_f64().unwrap(), r[1].as_i64().unwrap() as i32, r[2].as_f64().unwrap());
        assert_eq!(round_nd(v, nd), want, "round({v}, {nd})");
    }
}

#[test]
fn geometry_views() {
    let f = fixture("geometry");
    let clip: Clip = serde_json::from_value(f["clip"].clone()).unwrap();
    let tilt: Tilt = serde_json::from_value(f["tilt"].clone()).unwrap();
    for s in f["samples"].as_array().unwrap() {
        let t = s["t"].as_f64().unwrap();
        let v = geometry::clip_view_at(&clip, t);
        close(&[v.yaw, v.pitch, v.roll, v.fov], &vec_f(&s["view"]), 1e-12, &format!("vue à {t}"));
        let m = geometry::view_matrix(v.yaw, v.pitch, Some(&geometry::tilt_matrix(Some(&tilt))), v.roll);
        let want: Vec<f64> = s["m"].as_array().unwrap().iter().flat_map(vec_f).collect();
        close(&m.iter().flatten().copied().collect::<Vec<_>>(), &want, 1e-12, "matrice");
        let (a, b, c) = geometry::v360_angles(&m);
        close(&[a, b, c], &vec_f(&s["v360"]), 1e-9, "v360");
        let r = geometry::min_rotation(m[1]);
        let want: Vec<f64> = s["minrot"].as_array().unwrap().iter().flat_map(vec_f).collect();
        close(&r.iter().flatten().copied().collect::<Vec<_>>(), &want, 1e-12, "min_rotation");
    }
}

#[test]
fn analyze_pipeline() {
    let f = fixture("analyze");
    let g = &f["gps"];
    let gps = Gps { t: vec_f(&g["t"]), lat: vec_f(&g["lat"]), lon: vec_f(&g["lon"]), speed: vec_f(&g["speed"]),
                    alt: vec_f(&g["alt"]), heading: vec_f(&g["heading"]) };
    let (vib, gyro) = (vec_f(&f["vib"]), vec_f(&f["gyro"]));
    let t0 = f["t0"].as_f64().unwrap();
    let (corr, off) = analyze::auto_offset(&gps, t0, &vib);
    close(&[corr, off], &[f["corr"].as_f64().unwrap(), f["offset"].as_f64().unwrap()], 1e-9, "synchro");
    let n = vib.len();
    let times: Vec<f64> = (0..n).map(|k| t0 + off + k as f64).collect();
    let (s, valid) = analyze::sample_gps(&gps, &times);
    let want_valid: Vec<bool> = f["valid"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
    assert_eq!(valid, want_valid);
    for k in ["lat", "lon", "speed", "alt", "heading"] {
        close(&s[k], &vec_f(&f["sampled"][k]), 1e-12, k);
    }
    let (score, turn, climb, cands) = analyze::score_and_candidates(n, &vib, &gyro, &s, &valid);
    close(&score, &vec_f(&f["score"]), 1e-9, "score");
    close(&turn, &vec_f(&f["turn"]), 1e-9, "turn");
    close(&climb, &vec_f(&f["climb"]), 1e-9, "climb");
    assert_eq!(json!(cands), f["candidates"]);
    let none: HashMap<&str, Vec<f64>> = ["lat", "lon", "speed", "alt", "heading"].into_iter().map(|k| (k, vec![f64::NAN; n])).collect();
    let (score, _, _, cands) = analyze::score_and_candidates(n, &vib, &gyro, &none, &vec![false; n]);
    close(&score, &vec_f(&f["score_nogps"]), 1e-9, "score sans GPS");
    assert_eq!(json!(cands), f["candidates_nogps"]);
    let stats = analyze::ride_stats(n, &s, &valid);
    for (k, want) in f["stats"].as_object().unwrap() {
        close(&[stats[k].as_f64().unwrap()], &[want.as_f64().unwrap()], 1e-12, k);
    }
    for case in f["tilt"].as_array().unwrap() {
        let gv = vec_f(&case[0]);
        let t = analyze::mount_tilt([gv[0], gv[1], gv[2]]);
        close(&[t.pitch, t.roll], &[case[1]["pitch"].as_f64().unwrap(), case[1]["roll"].as_f64().unwrap()], 1e-12, "tilt");
    }
}

/// Analyse minimale (séries score/vitesse) complétée de valeurs neutres.
fn analysis(v: &Value) -> Analysis {
    serde_json::from_value(json!({
        "id": v["id"], "date": "", "time": "", "key": [], "override": null, "utc_t0": 0.0, "offset_s": 0.0,
        "offset_source": "aucun", "corr": null, "duration": v["duration"], "gps_coverage": 0.0, "segments": [],
        "series": v["series"], "candidates": [], "version": analyze::CACHE_VERSION,
        "tilt": {"pitch": 0.0, "roll": 0.0}, "stats": {},
    })).unwrap()
}

#[test]
fn hyperlapse_and_automontage() {
    let f = fixture("montage");
    let results: BTreeMap<String, Analysis> =
        f["results"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), analysis(v))).collect();
    for (sid, want) in f["hyperlapse"].as_object().unwrap() {
        let r = &results[sid];
        close(&hyperlapse::density(r, 120.0), &vec_f(&want["density"]), 1e-9, "densité");
        close(&hyperlapse::frame_times(r, 120.0), &vec_f(&want["times"]), 1e-9, "instants");
        let s = hyperlapse::summary(r, 120.0);
        assert_eq!(json!(s), want["summary"]);
    }
    let existing: HashMap<String, Vec<Clip>> = serde_json::from_value(f["existing"].clone()).unwrap();
    for (key, want) in f["plans"].as_object().unwrap() {
        let (target, ex) = match key.split_once('_') {
            Some((d, _)) => (d.parse::<f64>().unwrap(), existing.clone()),
            None => (key.parse::<f64>().unwrap(), HashMap::new()),
        };
        let plan = automontage::plan(&results, target, &ex, 0.6);
        let got: BTreeMap<&String, Vec<Value>> = plan.iter()
            .map(|(sid, cl)| (sid, cl.iter().map(|c| json!({"start": c.start, "end": c.end})).collect())).collect();
        assert_eq!(json!(got), *want, "plan {key}");
    }
}

#[test]
fn horizon_emission_and_viterbi() {
    let f = fixture("horizon");
    let img = image::open(format!("{}/tests/fixtures/horizon.png", env!("CARGO_MANIFEST_DIR"))).unwrap().to_luma8();
    let img: Vec<f32> = img.pixels().map(|p| p.0[0] as f32).collect();
    let e: Vec<f64> = horizon::emission(&img, None).iter().map(|v| *v as f64).collect();
    // contours de même intensité en limite des MAX_EDGES retenus : sélection arbitraire côté NumPy
    close(&e, &vec_f(&f["emission"]), 1e-3, "émission");
    let argmax = |v: &[f64]| (0..v.len()).fold(0, |m, k| if v[k] > v[m] { k } else { m });
    assert_eq!(argmax(&e), argmax(&vec_f(&f["emission"])), "état le plus probable");
    let ns = horizon::n_states();
    let big: Vec<f32> = f["E"].as_array().unwrap().iter().flat_map(|row| vec_f(row).into_iter().map(|v| v as f32)).collect();
    let (speed, lean) = (vec_f(&f["speed"]), vec_f(&f["lean"]));
    let neg: Vec<f64> = lean.iter().map(|v| -v).collect();
    let prior = horizon::speed_prior(&speed, &neg);
    for (k, want) in f["prior_rows"].as_object().unwrap() {
        let k: usize = k.parse().unwrap();
        let row: Vec<f64> = prior[k * ns..(k + 1) * ns].iter().map(|v| *v as f64).collect();
        close(&row, &vec_f(want), 1e-6, "a priori");
    }
    let path: Vec<usize> = horizon::viterbi(&big, &prior);
    assert_eq!(json!(path), f["path"]);
}
