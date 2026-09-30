"""Références des tests de non-régression Rust, calculées par les modules Python d'origine.

Données synthétiques (graine fixe) : aucune donnée personnelle. À relancer depuis la racine :
    PYTHONPATH=. python3 core/tests/gen_fixtures.py
"""
import json
from pathlib import Path

import numpy as np
from PIL import Image

import analyze
import automontage
import geometry
import horizon
import hyperlapse

OUT = Path(__file__).parent / "fixtures"
rng = np.random.default_rng(42)
nan_list = lambda a: [None if not np.isfinite(v) else float(v) for v in np.asarray(a, float)]

# --- numeric
x = rng.normal(0, 1, 500).cumsum()
x_nan = x.copy()
x_nan[[3, 50, 51, 200]] = np.nan
ang = np.radians((np.linspace(0, 1500, 300) + rng.normal(0, 30, 300)) % 360)
numeric = {
    "x": x.tolist(), "x_nan": nan_list(x_nan), "ang": ang.tolist(),
    "smooth5": analyze._smooth(x_nan, 5).tolist(), "smooth30": analyze._smooth(x, 30).tolist(),
    "rank": analyze._rank(np.round(x_nan, 0)).tolist(),
    "gradient": np.gradient(x).tolist(),
    "p15": float(np.percentile(x, 15)), "p99_5": float(np.percentile(x, 99.5)),
    "nanmedian": float(np.nanmedian(x_nan)),
    "interp_at": np.linspace(-5, 505, 97).tolist(),
    "interp": np.interp(np.linspace(-5, 505, 97), np.arange(500) * 1.0, x).tolist(),
    "unwrap": np.unwrap(ang).tolist(),
    "corr": float(np.corrcoef(x[:-1], x[1:])[0, 1]),
    "gauss15": horizon._gauss(x, 15).tolist(), "gauss1_5": horizon._gauss(x, 1.5).tolist(),
    "round": [[v, nd, round(v, nd)] for v in (2.675, 1.005, -0.125, 0.5, 1.5, 45.192521499, 69.45) for nd in (0, 1, 2, 3, 6)],
}
(OUT / "numeric.json").write_text(json.dumps(numeric))

# --- géométrie : cadrage d'un clip à points clés, matrices et angles v360
clip = {"start": 10, "end": 30, "yaw": 20, "pitch": -10, "fov": 100,
        "keyframes": [{"t": 0, "yaw": 170, "pitch": -5, "roll": 0, "fov": 90, "curve": "ease_in_out"},
                      {"t": 8, "yaw": -150, "pitch": 10, "roll": 12, "fov": 110, "curve": "quick"},
                      {"t": 15, "yaw": 0, "pitch": 0, "roll": -5, "fov": 100}]}
tilt = {"pitch": 7.5, "roll": -3.25}
geo = []
for t in np.linspace(-1, 21, 45):
    v = geometry.clip_view_at(clip, t)
    m = geometry.view_matrix(v["yaw"], v["pitch"], geometry.tilt_matrix(tilt), v["roll"])
    geo.append({"t": float(t), "view": [v["yaw"], v["pitch"], v["roll"], v["fov"]], "m": np.asarray(m).tolist(),
                "v360": list(geometry.v360_angles(m)), "minrot": np.asarray(geometry.min_rotation(m[1])).tolist()})
(OUT / "geometry.json").write_text(json.dumps({"clip": clip, "tilt": tilt, "samples": geo}))

# --- analyse : GPS synthétique, synchro, score, candidats, statistiques
n = 1800
t0 = 1_780_000_000.0
true_off = -12.5
tg = np.sort(t0 + true_off + rng.uniform(-60, n + 60, 1500))
tg = tg[~((tg > t0 + 700) & (tg < t0 + 760))]            # tunnel : pas de positions
speed_kn = np.clip(35 + 20 * np.sin((tg - t0) / 90) + rng.normal(0, 2, len(tg)), 0, None)
speed_kn[(tg > t0 + 1200) & (tg < t0 + 1300)] = 0          # arrêt
heading = (np.cumsum(rng.normal(0, 8, len(tg))) + 360) % 360
lat = 45.2 + np.cumsum(speed_kn) * 1e-5
lon = 6.7 + np.cumsum(np.cos(np.radians(heading))) * 1e-4
alt = 800 + 300 * np.sin((tg - t0) / 400)
gps = {"t": tg, "lat": lat, "lon": lon, "speed": speed_kn * analyze.KNOTS_TO_KMH, "alt": alt,
       "heading": np.degrees(np.unwrap(np.radians(heading)))}
tt = np.arange(n)
g_true, _ = analyze.sample_gps(gps, t0 + true_off + tt)
vib = np.nan_to_num(g_true["speed"]) / 100 + rng.normal(0, 0.05, n) ** 2
gyro = np.abs(rng.normal(0, 300, n)) + np.nan_to_num(np.abs(np.gradient(g_true["heading"]))) * 50
corr, off = analyze.auto_offset(gps, t0, vib)
g, valid = analyze.sample_gps(gps, t0 + off + tt)
score, turn, climb, cands = analyze.score_and_candidates(n, vib, gyro, g, valid)
g_none = {k: np.full(n, np.nan) for k in ("lat", "lon", "speed", "alt", "heading")}
score_ng, _, _, cands_ng = analyze.score_and_candidates(n, vib, gyro, g_none, np.zeros(n, bool))
fixture = {
    "t0": t0, "gps": {k: v.tolist() for k, v in gps.items()}, "vib": vib.tolist(), "gyro": gyro.tolist(),
    "corr": corr, "offset": off, "valid": valid.tolist(),
    "sampled": {k: nan_list(v) for k, v in g.items()},
    "score": score.tolist(), "turn": nan_list(turn), "climb": nan_list(climb), "candidates": cands,
    "score_nogps": score_ng.tolist(), "candidates_nogps": cands_ng,
    "stats": analyze.ride_stats(tt, g, valid),
    "tilt": [[gv, analyze.mount_tilt(np.array(gv))] for gv in ([0.1, -0.2, -0.97], [-0.9, 0.3, 0.2], [0.0, 1.0, 0.0])],
}
(OUT / "analyze.json").write_text(json.dumps(fixture))

# --- résultat d'analyse complet synthétique (hyperlapse, montage auto) : séries arrondies comme le cache
def result(sid, n, seed):
    r = np.random.default_rng(seed)
    sc = np.clip(analyze._smooth(r.uniform(0, 1, n) ** 3 * 2, 25), 0, 1)
    sp = np.clip(60 + 30 * np.sin(np.arange(n) / 200) + r.normal(0, 5, n), 0, None)
    sp[n // 3:n // 3 + 120] = 0
    return {"id": sid, "duration": n, "series": {"score": analyze._clean(sc, 3), "speed": analyze._clean(sp, 1)}}
results = {"VID_A": result("VID_A", 3600, 1), "VID_B": result("VID_B", 1500, 2), "VID_C": result("VID_C", 90, 3)}
hl = {sid: {"density": hyperlapse.density(r, 120).tolist(), "times": hyperlapse.frame_times(r, 120).tolist(),
            "summary": hyperlapse.summary(r, 120)} for sid, r in results.items() if r["duration"] > 1000}
_argsort = np.argsort
automontage.np.argsort = lambda a, **k: _argsort(a, kind="stable")   # égalités : même ordre que Rust
plans = {str(d): {sid: [{k: c[k] for k in ("start", "end")} for c in cl]
                  for sid, cl in automontage.plan(results, d, {}).items()} for d in (60, 300)}
existing = {"VID_A": [{"start": 1000, "end": 1100}]}
plans["300_existing"] = {sid: [{k: c[k] for k in ("start", "end")} for c in cl]
                         for sid, cl in automontage.plan(results, 300, existing).items()}
automontage.np.argsort = _argsort
(OUT / "montage.json").write_text(json.dumps({"results": results, "hyperlapse": hl, "plans": plans, "existing": existing}))

# --- horizon : émissions d'une image synthétique, a priori et Viterbi
img = np.zeros((horizon.H, horizon.W), np.float32)
yy, xx = np.mgrid[0:horizon.H, 0:horizon.W]
img += 60 * ((xx // 37) % 2) + 40 * ((yy + xx // 5) // 23 % 2) + rng.normal(0, 3, img.shape).astype(np.float32)
img = np.clip(img, 0, 255).astype(np.uint8)
Image.fromarray(img, "L").save(OUT / "horizon.png")
img = img.astype(np.float32)
T, S = 120, len(horizon.STATES)
E = np.log(rng.uniform(0.01, 1, (T, S))).astype(np.float32)
E[:, 22 * 13 + 6] += 3.0                                    # état (0°, 0°) favorisé
E[50:80, 30 * 13 + 7] += 5.0                                # puis un virage
speed = np.clip(np.linspace(0, 90, T) + rng.normal(0, 3, T), 0, None)
lean = 20 * np.sin(np.linspace(0, 3, T))
E = np.round(E, 5).astype(np.float32)
prior = horizon.speed_prior(speed, -lean)
(OUT / "horizon.json").write_text(json.dumps({
    "emission": horizon.emission(img).tolist(),
    "E": np.round(E, 5).astype(np.float32).tolist(), "speed": speed.tolist(), "lean": lean.tolist(), "prior_rows": {str(k): prior[k].tolist() for k in (0, 60, T - 1)},
    "path": horizon.viterbi(E, prior).tolist(),
}))
print("références écrites dans", OUT)
