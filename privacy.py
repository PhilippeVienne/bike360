"""Confidentialité : détection des visages et plaques, suivi, floutage à l'export.

Analyse : chaque clip est rendu (moteur GPU) dans son cadrage en 1920×1080 ; on y détecte
visages (YuNet, image entière) et plaques (YOLOv9, tuiles 2×2 : les plaques de moto sont
petites), puis on relie les détections d'image en image en pistes. Chaque piste est stockée
en directions dans le repère caméra (sphère) et en demi-angles : le floutage suit donc
quel que soit le cadrage, le format de sortie ou le temps (hyperlapse).

Export : les pistes actives sont reprojetées dans chaque image de sortie et floutées
(flou gaussien fort, marge) sur le rendu, avant les incrustations.
"""
import json
import math
import subprocess
import warnings
from pathlib import Path

import numpy as np

import analyze

MODELS = analyze.CACHE / "models"
FACE_MODEL = MODELS / "face_detection_yunet_2023mar.onnx"
PLATE_MODEL = "yolo-v9-t-640-license-plate-end2end"
DATA = analyze.DATA / "privacy"
THUMBS = analyze.CACHE / "privacy"
AW, AH = 1920, 1080               # rendu d'analyse (cadrage et champ du clip)
DETECT_EVERY = 2                  # une image sur deux : le suivi comble
FACE_SCORE, PLATE_SCORE = 0.6, 0.35
TRACK_GAP = 12                    # images sans détection tolérées dans une piste
MIN_HITS, SURE_CONF = 2, 0.75     # piste retenue si ≥ 2 détections, ou une seule très sûre
EXTEND_S = 0.25                   # floutage prolongé avant/après la piste
PAD = 0.3                         # marge autour de la zone détectée


class Detector:
    """Visages (YuNet) + plaques (YOLOv9 en tuiles), sur images BGR."""

    def __init__(self):
        import cv2
        from open_image_models import create_detector
        warnings.filterwarnings("ignore")
        self.cv2 = cv2
        self.face = cv2.FaceDetectorYN.create(str(FACE_MODEL), "", (AW, AH), FACE_SCORE, 0.3, 5000)
        self.plates = create_detector(PLATE_MODEL, conf_thresh=PLATE_SCORE)

    def __call__(self, img):
        cv2 = self.cv2
        h, w = img.shape[:2]
        out = []
        self.face.setInputSize((w, h))
        _, faces = self.face.detect(img)
        for f in [] if faces is None else faces:
            out.append(("visage", float(f[-1]), *(float(v) for v in f[:4])))
        # tuiles 2×2 avec recouvrement : une plaque de moto fait ~2 % de la largeur
        ov = 100
        boxes, confs = [], []
        for ty in (0, h // 2 - ov):
            for tx in (0, w // 2 - ov):
                crop = img[ty:ty + h // 2 + ov, tx:tx + w // 2 + ov]
                for d in self.plates.predict(np.ascontiguousarray(crop)):
                    b = d.bounding_box
                    boxes.append([b.x1 + tx, b.y1 + ty, b.width, b.height])
                    confs.append(float(d.confidence))
        for i in (cv2.dnn.NMSBoxes(boxes, confs, PLATE_SCORE, 0.3) if boxes else []):
            out.append(("plaque", confs[i], *map(float, boxes[i])))
        return out


def _iou(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    ix = max(0, min(ax + aw, bx + bw) - max(ax, bx))
    iy = max(0, min(ay + ah, by + bh) - max(ay, by))
    inter = ix * iy
    return inter / (aw * ah + bw * bh - inter + 1e-9)


def link(frames):
    """Relie les détections [(indice d'image, [(type, conf, x, y, w, h)])] en pistes."""
    tracks, active = [], []
    for fi, dets in frames:
        active = [t for t in active if fi - t["hits"][-1][0] <= TRACK_GAP]
        used = set()
        for kind, conf, *box in sorted(dets, key=lambda d: -d[1]):
            best, score = None, 0.0
            for t in active:
                if t["kind"] != kind or id(t) in used:
                    continue
                last = t["hits"][-1][2]
                # recouvrement, ou centre proche (objet rapide entre deux détections)
                cx, cy = box[0] + box[2] / 2, box[1] + box[3] / 2
                lx, ly = last[0] + last[2] / 2, last[1] + last[3] / 2
                near = math.hypot(cx - lx, cy - ly) < 1.5 * max(box[2], box[3], last[2], last[3])
                s = _iou(box, last) + (0.1 if near else 0)
                if s > score and (s > 0.15 or near):
                    best, score = t, s
            if best is None:
                best = {"kind": kind, "hits": []}
                tracks.append(best)
                active.append(best)
            best["hits"].append((fi, conf, box))
            used.add(id(best))
    return [t for t in tracks if len(t["hits"]) >= MIN_HITS or max(h[1] for h in t["hits"]) >= SURE_CONF]


# ------------------------------------------------------------------ géométrie écran ↔ sphère

def _ray(u, v, W, H, hfov):
    """Rayon (repère vue) du pixel (u, v) ; x droite, y haut, z devant."""
    th = math.tan(math.radians(hfov) / 2)
    tv = th * H / W
    return np.array([(2 * u / W - 1) * th, -(2 * v / H - 1) * tv, 1.0])


def box_to_sphere(box, M, hfov, W, H):
    """Boîte écran → (direction caméra unitaire, demi-angle horizontal, vertical) en radians."""
    x, y, w, h = box
    c = _ray(x + w / 2, y + h / 2, W, H, hfov)
    ex, ey = _ray(x + w, y + h / 2, W, H, hfov), _ray(x + w / 2, y + h, W, H, hfov)
    ang = lambda a, b: math.acos(max(-1.0, min(1.0, a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))))
    d = M @ (c / np.linalg.norm(c))
    return d / np.linalg.norm(d), ang(c, ex), ang(c, ey)


def sphere_to_box(d, ax, ay, M, hfov, W, H):
    """Direction caméra + demi-angles → boîte écran (x, y, w, h) avec marge, ou None si derrière."""
    v = M.T @ d
    if v[2] <= 0.05:
        return None
    th = math.tan(math.radians(hfov) / 2)
    tv = th * H / W
    u = (v[0] / v[2] / th + 1) * W / 2
    y = (1 - v[1] / v[2] / tv) * H / 2
    # taille : demi-angle rapporté au champ au centre de la zone (perspective incluse)
    r = math.hypot(v[0] / v[2], v[1] / v[2])
    stretch = 1 + r * r
    hw = math.tan(ax) * stretch / th * W / 2 * (1 + PAD)
    hh = math.tan(ay) * stretch / tv * H / 2 * (1 + PAD)
    return u - hw, y - hh, 2 * hw, 2 * hh


def to_samples(track, times, mats, fovs):
    """Piste écran → échantillons sphère [(t session, dx, dy, dz, ax, ay)]."""
    out = []
    for fi, _, box in track["hits"]:
        d, ax, ay = box_to_sphere(box, mats[fi], fovs[fi], AW, AH)
        out.append([round(float(times[fi]), 3), *(round(float(x), 5) for x in d), round(ax, 5), round(ay, 5)])
    return out


MERGE_GAP_S, MERGE_ANGLE = 0.35, 30.0   # recollage : objets rapides (voiture croisée de près)


def merge_fragments(tracks):
    """Recolle sur la sphère les fragments d'un même objet (même type, qui se suivent de près).

    Un véhicule croisé de près traverse l'image de plusieurs degrés par image : le suivi à
    l'écran le coupe en morceaux, ce qui laisserait la plaque nette entre deux morceaux.
    """
    tracks = sorted(tracks, key=lambda t: t["samples"][0][0])
    out = []
    for t in tracks:
        s0 = t["samples"][0]
        best, best_ang = None, None
        for o in out:
            e = o["samples"][-1]
            if o["kind"] != t["kind"] or not -0.15 <= s0[0] - e[0] <= MERGE_GAP_S:
                continue
            ang = math.degrees(math.acos(max(-1.0, min(1.0, float(np.dot(e[1:4], s0[1:4]))))))
            if ang <= MERGE_ANGLE and (best_ang is None or ang < best_ang):
                best, best_ang = o, ang
        if best is None:
            out.append(t)
            continue
        best["samples"] = sorted(best["samples"] + t["samples"], key=lambda x: x[0])
        best["conf"] = max(best["conf"], t["conf"])
        if t["conf"] > best.get("thumb_conf", 0):   # garder la vignette la plus sûre
            best["thumb"], best["thumb_conf"] = t["thumb"], t["conf"]
    for k, t in enumerate(out):
        t["id"] = k
        t.pop("thumb_conf", None)
    return out


def regions_at(tracks, t):
    """Zones actives à l'instant t (session) : [(direction, ax, ay)], interpolées entre échantillons."""
    out = []
    for tr in tracks:
        s = tr["samples"]
        if not tr.get("enabled", True) or not s or not s[0][0] - EXTEND_S <= t <= s[-1][0] + EXTEND_S:
            continue
        ts = [x[0] for x in s]
        k = int(np.searchsorted(ts, t))
        if k <= 0 or k >= len(s):
            a = s[0] if k <= 0 else s[-1]
            d, ax, ay = np.array(a[1:4]), a[4], a[5]
        else:
            a, b = s[k - 1], s[k]
            f = (t - a[0]) / max(b[0] - a[0], 1e-6)
            if b[0] - a[0] > TRACK_GAP / 15:   # trou trop long : pas d'invention entre deux passages
                continue
            d = np.array(a[1:4]) * (1 - f) + np.array(b[1:4]) * f
            ax, ay = a[4] * (1 - f) + b[4] * f, a[5] * (1 - f) + b[5] * f
        out.append((d / np.linalg.norm(d), ax, ay))
    return out


# ------------------------------------------------------------------ fichiers

def path(sid):
    return DATA / f"{sid}.json"


def load(sid):
    return json.loads(path(sid).read_text()) if path(sid).exists() else {}


def save(sid, data):
    DATA.mkdir(parents=True, exist_ok=True)
    path(sid).write_text(json.dumps(data))


def view_key(clip):
    """Empreinte du cadrage/temps d'un clip : l'analyse est à refaire si elle change."""
    keys = ("start", "end", "yaw", "pitch", "roll", "fov", "horizon", "keyframes")
    return json.dumps({k: clip.get(k) for k in keys}, sort_keys=True)


# ------------------------------------------------------------------ analyse d'un clip

def analyze_clip(render_h264, times, mats, fovs, sid, clip_id, detector, progress=None):
    """Détecte et suit dans le rendu d'analyse (`times`, `mats`, `fovs` : une entrée par image).

    Retourne les pistes (avec vignette de la meilleure détection).
    """
    import cv2
    n = len(times)
    proc = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(render_h264), "-f", "rawvideo",
                             "-pix_fmt", "bgr24", "-"], stdout=subprocess.PIPE)
    frames, crops = [], {}
    size = AW * AH * 3
    for fi in range(n):
        buf = proc.stdout.read(size)
        if len(buf) < size:
            break
        if fi % DETECT_EVERY:
            continue
        img = np.frombuffer(buf, np.uint8).reshape(AH, AW, 3)
        dets = detector(img)
        frames.append((fi, dets))
        for kind, conf, *box in dets:   # petite vue de chaque zone, pour la vignette de revue
            x, y, w, h = box
            m = 0.6 * max(w, h)
            crop = img[int(max(0, y - m)):int(min(AH, y + h + m)), int(max(0, x - m)):int(min(AW, x + w + m))]
            if crop.size:
                crops[(fi, tuple(box))] = cv2.resize(crop, (120, max(1, int(120 * crop.shape[0] / crop.shape[1]))))
        if progress:
            progress(fi / n)
    proc.stdout.close()
    proc.wait()

    tracks = []
    THUMBS.mkdir(parents=True, exist_ok=True)
    for k, t in enumerate(link(frames)):
        fi, conf, box = max(t["hits"], key=lambda h: h[1] * h[2][2] * h[2][3])
        thumb = f"{sid}_{clip_id}_{k}.jpg"
        if (fi, tuple(box)) in crops:
            cv2.imwrite(str(THUMBS / thumb), crops[(fi, tuple(box))])
        conf = round(max(h[1] for h in t["hits"]), 2)
        tracks.append({"id": k, "kind": t["kind"], "conf": conf, "thumb_conf": conf,
                       "enabled": True, "thumb": thumb, "samples": to_samples(t, times, mats, fovs)})
    return merge_fragments(tracks)


# ------------------------------------------------------------------ floutage à l'export

def blur_video(src, dst, times, mats, fovs, tracks, W, H, encoder_args):
    """Floute les zones des pistes dans `src` (une entrée times/mats/fovs par image) → `dst`.

    Retourne le nombre d'images modifiées. Le son est recopié tel quel.
    """
    import cv2
    dec = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(src), "-f", "rawvideo", "-pix_fmt", "bgr24", "-"],
                           stdout=subprocess.PIPE)
    enc = subprocess.Popen(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgr24", "-s", f"{W}x{H}",
                            "-r", "30000/1001", "-i", "-", "-i", str(src), "-map", "0:v", "-map", "1:a?",
                            *encoder_args, "-c:a", "copy", "-movflags", "+faststart", str(dst)],
                           stdin=subprocess.PIPE)
    size, touched, fi = W * H * 3, 0, 0
    while True:
        buf = dec.stdout.read(size)
        if len(buf) < size:
            break
        k = min(fi, len(times) - 1)
        regions = regions_at(tracks, times[k])
        if regions:
            img = np.frombuffer(buf, np.uint8).reshape(H, W, 3).copy()
            for d, ax, ay in regions:
                box = sphere_to_box(d, ax, ay, mats[k], fovs[k], W, H)
                if box is None:
                    continue
                x0, y0 = int(max(0, box[0])), int(max(0, box[1]))
                x1, y1 = int(min(W, box[0] + box[2])), int(min(H, box[1] + box[3]))
                if x1 - x0 < 2 or y1 - y0 < 2:
                    continue
                roi = img[y0:y1, x0:x1]
                k_size = max(3, (max(x1 - x0, y1 - y0) // 3) | 1)
                img[y0:y1, x0:x1] = cv2.GaussianBlur(cv2.GaussianBlur(roi, (k_size, k_size), 0), (k_size, k_size), 0)
            buf = img.tobytes()
            touched += 1
        enc.stdin.write(buf)
        fi += 1
    dec.stdout.close()
    dec.wait()
    enc.stdin.close()
    if enc.wait() != 0:
        raise RuntimeError("floutage : échec de l'encodage")
    return touched
