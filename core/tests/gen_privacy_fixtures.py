"""Références des tests Rust du module de confidentialité, calculées par privacy.py et OpenCV.

Données synthétiques (graine fixe) : aucune donnée personnelle, aucun modèle ni GPU. À relancer
depuis la racine :
    PYTHONPATH=. python3 core/tests/gen_privacy_fixtures.py
"""
import json
import math
from pathlib import Path

import cv2
import numpy as np

import geometry
import privacy

OUT = Path(__file__).parent / "fixtures"
rng = np.random.default_rng(7)

# --- traitement d'image : bruit + formes claires, redimensionnements des modèles et du suiveur
img = rng.integers(0, 256, (61, 97, 3), dtype=np.uint8)
img = cv2.GaussianBlur(img, (5, 5), 0)
cv2.rectangle(img, (10, 12), (30, 24), (235, 238, 240), -1)
cv2.rectangle(img, (50, 30), (58, 50), (220, 225, 230), -1)
cv2.circle(img, (80, 15), 6, (250, 250, 250), -1)
sizes = [(40, 25), (64, 64), (120, 76), (33, 97), (150, 95), (7, 5)]
resize = [{"w": w, "h": h, "out": cv2.resize(img, (w, h)).flatten().tolist()} for w, h in sizes]
area = cv2.resize(img[:60, :96], (48, 30), interpolation=cv2.INTER_AREA).flatten().tolist()
hsv = cv2.cvtColor(img, cv2.COLOR_BGR2HSV)
mask = ((hsv[..., 2] > 150) & (hsv[..., 1] < 70)).astype(np.uint8)
n, _, stats, _ = cv2.connectedComponentsWithStats(mask)
templ = np.ascontiguousarray(img[20:34, 40:62])
_, best, _, loc = cv2.minMaxLoc(cv2.matchTemplate(img, templ, cv2.TM_CCOEFF_NORMED))
blur = {k: cv2.GaussianBlur(img, (k, k), 0).flatten().tolist() for k in (3, 5, 9, 15)}
image = {"w": 97, "h": 61, "img": img.flatten().tolist(), "resize": resize, "area2": area, "mask": mask.flatten().tolist(),
         "components": stats[1:].tolist(), "match": [best, loc[0], loc[1]], "blur": blur}

# --- géométrie écran ↔ sphère
tilt = geometry.tilt_matrix({"pitch": -12.0, "roll": 4.5})
geo = []
for k in range(40):
    M = geometry.view_matrix(rng.uniform(-180, 180), rng.uniform(-30, 30), tilt, rng.uniform(-10, 10))
    fov = rng.uniform(60, 120)
    W, H = [(1920, 1080), (1080, 1920), (400, 400)][k % 3]
    box = (rng.uniform(0, W - 50), rng.uniform(0, H - 50), rng.uniform(4, 200), rng.uniform(4, 120))
    d, ax, ay = privacy.box_to_sphere(box, M, fov, W, H)
    geo.append({"M": np.asarray(M).tolist(), "fov": fov, "W": W, "H": H, "box": list(box), "d": d.tolist(), "ax": ax, "ay": ay,
                "back": privacy.sphere_to_box(d, ax, ay, M, fov, W, H), "tight": privacy.tight_box(d, ax, ay, M, fov, W) if W == H else None,
                "local_view": np.asarray(privacy.local_view(d)).tolist(), "local_fov": privacy.local_fov(ax, ay)})

# --- pistes : fragments à recoller, zones actives, zones par image
def track(kind, t0, n, d0, drift, conf, gap=1 / 15):
    d = np.array(d0, float) / np.linalg.norm(d0)
    s = []
    for i in range(n):
        v = d + np.array(drift) * i
        v /= np.linalg.norm(v)
        s.append([round(t0 + i * gap, 3), *(round(float(x), 5) for x in v), 0.02 + 0.001 * i, 0.012])
    return {"id": 0, "kind": kind, "conf": conf, "thumb_conf": conf, "enabled": True, "thumb": f"t{t0}.jpg", "samples": s}

frags = [track("plaque", 10.0, 12, [0.2, -0.1, 1], [0.01, 0, 0], 0.8),
         track("plaque", 10.9, 10, [0.33, -0.1, 1], [0.01, 0, 0], 0.9),       # recollé (0,17 s, proche)
         track("plaque", 12.0, 8, [0.2, -0.1, 1], [0, 0, 0], 0.6),            # tenu (trou 1 s, même direction ?)
         track("visage", 10.2, 15, [-0.5, 0.1, 1], [0, 0.005, 0], 0.7),
         track("plaque", 10.5, 6, [-0.9, 0, 0.4], [0, 0, 0], 0.5),            # autre objet
         track("plaque", 16.5, 5, [0.2, -0.1, 1], [0, 0, 0], 0.95),           # trop tard
         track("visage", 11.2, 5, [-0.49, 0.18, 1], [0, 0, 0], 0.72)]         # chevauche le visage
merged = privacy.merge_fragments(json.loads(json.dumps(frags)))
manual = {"id": "m1", "kind": "manuel", "conf": 1.0, "enabled": True, "thumb": None,
          "samples": [[9.0, 0, 0, 1, 0.05, 0.05], [9.5, 0.01, 0, 1, 0.05, 0.05], [13.0, 0.02, 0, 1, 0.05, 0.05],
                      [13.5, 0.3, 0, 1, 0.05, 0.05]]}
off = dict(frags[4], enabled=False)
tracks = merged + [manual, off]
times = list(np.arange(8.6, 17.8, 1 / 29.97))
mats = [np.asarray(geometry.view_matrix(15 * math.sin(k / 30), -5, tilt, 2 * math.cos(k / 20))) for k in range(len(times))]
fovs = [95 + 10 * math.sin(k / 40) for k in range(len(times))]
regions = [[[r[0].tolist(), r[1], r[2]] for r in privacy.regions_at(tracks, t)] for t in times[::7]]
boxes = {f"{W}x{H}": privacy.frame_boxes(times, mats, fovs, tracks, W, H) for W, H in [(1920, 1080), (1080, 1920)]}
fixed = [[round(float(t), 3), *(round(float(x), 5) for x in np.array([0.1, -0.2, 0.9]) / np.linalg.norm([0.1, -0.2, 0.9])), 0.03, 0.04]
         for t in np.arange(3.2, 7.1 + 0.5, 0.5)]

# --- ancienne liaison écran et empreintes de cadrage
dets = []
for f in range(0, 60, 2):
    d = [("plaque", 0.5 + 0.01 * (f % 7), 100.0 + 6 * f, 300.0, 40.0, 14.0), ("visage", 0.8, 900.0 - 3 * f, 200.0 + f, 30.0, 36.0)]
    if f % 10 == 0:
        d.append(("plaque", 0.9, 1500.0, 700.0, 60.0, 20.0))
    dets.append((f, d))
link = [{"kind": t["kind"], "hits": [[h[0], h[1], list(h[2])] for h in t["hits"]]} for t in privacy.link(dets)]
clips = [{"id": "a", "start": 10, "end": 25.5, "yaw": -12.25, "pitch": 0, "fov": 100, "horizon": "fixe"},
         {"id": "b", "start": 1.0e-05, "end": 3, "yaw": 1e16, "pitch": -0.0, "roll": 7, "fov": 99.99999999999999, "level": True,
          "keyframes": [{"t": 0, "yaw": 1, "curve": "ease_in", "é": "à\n"}, {"t": 2.5, "yaw": 2e-7, "pitch": 1}]},
         {"id": "c", "start": 3, "end": 4}]
keys = [privacy.view_key(c) for c in clips]

(OUT / "privacy.json").write_text(json.dumps({
    "image": image, "geometry": geo, "fragments": frags, "merged": merged, "tracks": tracks, "times": times,
    "mats": [m.tolist() for m in mats], "fovs": fovs, "regions": regions, "regions_step": 7, "boxes": boxes, "fixed": fixed,
    "detections": [[f, [[k, c, [x, y, w, h]] for k, c, x, y, w, h in d]] for f, d in dets], "link": link,
    "clips": clips, "keys": keys}))
