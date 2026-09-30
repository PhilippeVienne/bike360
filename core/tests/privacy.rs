//! Non-régression du module de confidentialité : résultats identiques à privacy.py et OpenCV sur
//! des données synthétiques (références : tests/gen_privacy_fixtures.py). Sans modèle ni GPU.

use bike360_core::geometry::Mat3;
use bike360_core::privacy::{self, imgproc, Image, Track};
use serde_json::Value;

fn fixture() -> Value {
    let path = format!("{}/tests/fixtures/privacy.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn bytes(v: &Value) -> Vec<u8> {
    v.as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u8).collect()
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

fn tracks(v: &Value) -> Vec<Track> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut t: Track = serde_json::from_value(t.clone()).unwrap();
            if let Some(c) = t.extra.remove("thumb_conf") {
                t.thumb_conf = c.as_f64().unwrap();
            }
            t
        })
        .collect()
}

#[test]
fn image_processing() {
    let fx = fixture();
    let im = &fx["image"];
    let (w, h) = (im["w"].as_u64().unwrap() as usize, im["h"].as_u64().unwrap() as usize);
    let img = Image::from_bgr(w, h, bytes(&im["img"]));
    for r in im["resize"].as_array().unwrap() {
        let (ow, oh) = (r["w"].as_u64().unwrap() as usize, r["h"].as_u64().unwrap() as usize);
        assert_eq!(imgproc::resize_linear(&img, ow, oh).data, bytes(&r["out"]), "resize {ow}×{oh}");
    }
    assert_eq!(imgproc::resize_area(&img.crop(0, 0, 96, 60), 48, 30).data, bytes(&im["area2"]), "INTER_AREA ×½");
    let mask = imgproc::light_mask(&img);
    assert_eq!(mask.iter().map(|m| *m as u8).collect::<Vec<_>>(), bytes(&im["mask"]), "masque TSV");
    let comps: Vec<[usize; 5]> = imgproc::components(&mask, w, h).iter().map(|c| [c.x, c.y, c.w, c.h, c.area]).collect();
    let want: Vec<[usize; 5]> = serde_json::from_value(im["components"].clone()).unwrap();
    assert_eq!(comps, want, "composantes connexes (ordre des étiquettes compris)");
    let templ = img.crop(40, 20, 62, 34);
    let (best, x, y) = imgproc::match_template_best(&img, &templ).unwrap();
    assert_eq!((x, y), (im["match"][1].as_u64().unwrap() as usize, im["match"][2].as_u64().unwrap() as usize));
    assert!((best - f(&im["match"][0])).abs() < 1e-5, "matchTemplate {best}");
    for (k, want) in im["blur"].as_object().unwrap() {
        let got = imgproc::gaussian_blur(&img, k.parse().unwrap()).data;
        let d = got.iter().zip(bytes(want)).map(|(a, b)| (*a as i32 - b as i32).abs()).max().unwrap();
        assert!(d <= 1, "GaussianBlur {k} : écart {d}");
    }
}

#[test]
fn sphere_geometry() {
    let fx = fixture();
    for g in fx["geometry"].as_array().unwrap() {
        let m: Mat3 = serde_json::from_value(g["M"].clone()).unwrap();
        let (fov, w, h) = (f(&g["fov"]), f(&g["W"]), f(&g["H"]));
        let b: [f64; 4] = serde_json::from_value(g["box"].clone()).unwrap();
        let (d, ax, ay) = privacy::box_to_sphere(&b, &m, fov, w, h);
        let want_d: [f64; 3] = serde_json::from_value(g["d"].clone()).unwrap();
        for k in 0..3 {
            assert!((d[k] - want_d[k]).abs() < 1e-12);
        }
        assert!((ax - f(&g["ax"])).abs() < 1e-12 && (ay - f(&g["ay"])).abs() < 1e-12);
        let back = privacy::sphere_to_box(&want_d, f(&g["ax"]), f(&g["ay"]), &m, fov, w, h);
        let want_back: Option<[f64; 4]> = serde_json::from_value(g["back"].clone()).unwrap();
        match (back, want_back) {
            (Some(a), Some(b)) => (0..4).for_each(|k| assert!((a[k] - b[k]).abs() < 1e-7, "sphere_to_box {a:?} {b:?}")),
            (a, b) => assert_eq!(a.is_some(), b.is_some()),
        }
        if let Some(t) = g["tight"].as_array() {
            let want: Vec<i32> = t.iter().map(|v| v.as_i64().unwrap() as i32).collect();
            let got = privacy::tight_box(&want_d, f(&g["ax"]), f(&g["ay"]), &m, fov, w as usize).unwrap();
            assert_eq!(got.to_vec(), want, "tight_box");
        }
        let lv = privacy::local_view(&want_d);
        let want_lv: Mat3 = serde_json::from_value(g["local_view"].clone()).unwrap();
        (0..9).for_each(|k| assert!((lv[k / 3][k % 3] - want_lv[k / 3][k % 3]).abs() < 1e-12));
        assert_eq!(privacy::local_fov(f(&g["ax"]), f(&g["ay"])), f(&g["local_fov"]));
    }
}

#[test]
fn tracks_regions_boxes() {
    let fx = fixture();
    let merged = privacy::merge_fragments(tracks(&fx["fragments"]));
    let got: Value = serde_json::to_value(&merged).unwrap();
    assert_eq!(got, fx["merged"], "merge_fragments");
    let all = tracks(&fx["tracks"]);
    let times: Vec<f64> = serde_json::from_value(fx["times"].clone()).unwrap();
    let mats: Vec<Mat3> = serde_json::from_value(fx["mats"].clone()).unwrap();
    let fovs: Vec<f64> = serde_json::from_value(fx["fovs"].clone()).unwrap();
    let step = fx["regions_step"].as_u64().unwrap() as usize;
    for (i, want) in fx["regions"].as_array().unwrap().iter().enumerate() {
        let got = privacy::regions_at(&all, times[i * step]);
        let want = want.as_array().unwrap();
        assert_eq!(got.len(), want.len(), "regions_at({})", times[i * step]);
        for ((d, ax, ay), w) in got.iter().zip(want) {
            let wd: [f64; 3] = serde_json::from_value(w[0].clone()).unwrap();
            (0..3).for_each(|k| assert!((d[k] - wd[k]).abs() < 1e-12));
            assert!((ax - f(&w[1])).abs() < 1e-12 && (ay - f(&w[2])).abs() < 1e-12);
        }
    }
    for (fmt, want) in fx["boxes"].as_object().unwrap() {
        let (w, h) = fmt.split_once('x').unwrap();
        let got = privacy::frame_boxes(&times, &mats, &fovs, &all, w.parse().unwrap(), h.parse().unwrap());
        assert_eq!(serde_json::to_value(&got).unwrap(), *want, "frame_boxes {fmt}");
    }
    let fixed = privacy::fixed_zone_samples(3.2, 7.1, &[0.1, -0.2, 0.9], 0.03, 0.04);
    assert_eq!(serde_json::to_value(&fixed).unwrap(), fx["fixed"], "fixed_zone_samples");
}

#[test]
fn link_and_keys() {
    let fx = fixture();
    let frames: Vec<(usize, Vec<privacy::Detection>)> = fx["detections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fr| {
            let dets = fr[1].as_array().unwrap().iter()
                .map(|d| privacy::Detection {
                    kind: if d[0] == "visage" { "visage" } else { "plaque" },
                    conf: f(&d[1]),
                    bbox: serde_json::from_value(d[2].clone()).unwrap(),
                })
                .collect();
            (fr[0].as_u64().unwrap() as usize, dets)
        })
        .collect();
    let got: Vec<Value> = privacy::link(&frames)
        .iter()
        .map(|t| serde_json::json!({"kind": t.kind, "hits": t.hits}))
        .collect();
    assert_eq!(Value::from(got), fx["link"], "link");
    for (c, k) in fx["clips"].as_array().unwrap().iter().zip(fx["keys"].as_array().unwrap()) {
        assert_eq!(privacy::view_key(c), k.as_str().unwrap(), "view_key");
    }
}
