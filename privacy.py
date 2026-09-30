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
TRACK_MODEL = MODELS / "object_tracking_vittrack_2023sep.onnx"   # suivi des zones tracées à la main
PLATE_MODEL = "yolo-v9-t-640-license-plate-end2end"
DATA = analyze.DATA / "privacy"
THUMBS = analyze.CACHE / "privacy"
AW, AH = 1920, 1080               # rendu d'analyse (cadrage et champ du clip)
DETECT_EVERY = 2                  # une image sur deux : le suivi comble
FACE_SCORE, PLATE_SCORE = 0.6, 0.35
FACE_SCALE = 0.5                  # détection des visages à mi-résolution (≈ 4× plus rapide)
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
        import onnxruntime as ort
        ort.set_default_logger_severity(3)
        self.face = cv2.FaceDetectorYN.create(str(FACE_MODEL), "", (AW, AH), FACE_SCORE, 0.3, 5000)
        # carte graphique si onnxruntime-gpu est utilisable (CUDA 12), sinon processeur
        providers = [p for p in ("CUDAExecutionProvider", "CPUExecutionProvider") if p in ort.get_available_providers()]
        self.plates = create_detector(PLATE_MODEL, conf_thresh=PLATE_SCORE, providers=providers, batch_size=5)

    def __call__(self, img):
        cv2 = self.cv2
        h, w = img.shape[:2]
        out = []
        # visages à mi-résolution : un visage trop petit pour y être vu n'est pas reconnaissable
        fs = FACE_SCALE
        small = cv2.resize(img, (int(w * fs), int(h * fs)), interpolation=cv2.INTER_AREA)
        self.face.setInputSize((small.shape[1], small.shape[0]))
        _, faces = self.face.detect(small)
        for f in [] if faces is None else faces:
            out.append(("visage", float(f[-1]), *(float(v) / fs for v in f[:4])))
        # image entière (plaques proches, qui chevauchent deux tuiles) + tuiles 2×2 avec
        # recouvrement (plaques de moto lointaines : ~2 % de la largeur)
        ov = 100
        views = [(0, 0, img)] + [(tx, ty, np.ascontiguousarray(img[ty:ty + h // 2 + ov, tx:tx + w // 2 + ov]))
                                 for ty in (0, h // 2 - ov) for tx in (0, w // 2 - ov)]
        boxes = []
        for (tx, ty, _), dets in zip(views, self.plates.predict([v for _, _, v in views])):   # un seul lot
            for d in dets:
                b = d.bounding_box
                boxes.append([b.x1 + tx, b.y1 + ty, b.width, b.height, float(d.confidence)])
        # fusion par union : une plaque vue en deux moitiés (bord de tuile) est floutée en entier
        merged = []
        for x, y, bw, bh, c in sorted(boxes, key=lambda b: -b[4]):
            for m in merged:
                ix = max(0, min(x + bw, m[0] + m[2]) - max(x, m[0]))
                iy = max(0, min(y + bh, m[1] + m[3]) - max(y, m[1]))
                if ix * iy > 0.2 * min(bw * bh, m[2] * m[3]):
                    x0, y0 = min(x, m[0]), min(y, m[1])
                    m[2], m[3] = max(x + bw, m[0] + m[2]) - x0, max(y + bh, m[1] + m[3]) - y0
                    m[0], m[1], m[4] = x0, y0, max(c, m[4])
                    break
            else:
                merged.append([x, y, bw, bh, c])
        for x, y, bw, bh, c in merged:
            out.append(("plaque", c, float(x), float(y), float(bw), float(bh)))
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
HOLD_GAP_S, HOLD_ANGLE = 7.0, 8.0       # … et objets suivis perdus quelques secondes (plaque en bord d'image,
                                        # cahots) : même direction → la zone est tenue entre les deux


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
            if o["kind"] != t["kind"] or not -0.15 <= s0[0] - e[0] <= HOLD_GAP_S:
                continue
            ang = math.degrees(math.acos(max(-1.0, min(1.0, float(np.dot(e[1:4], s0[1:4]))))))
            gap = s0[0] - e[0]
            ok = (gap <= MERGE_GAP_S and ang <= MERGE_ANGLE) or (gap <= HOLD_GAP_S and ang <= HOLD_ANGLE)
            if ok and (best_ang is None or ang < best_ang):
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


def all_tracks(entry):
    """Pistes détectées + zones tracées à la main d'un clip."""
    return (entry or {}).get("tracks", []) + (entry or {}).get("manual", [])


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
            gap = b[0] - a[0]
            if gap > TRACK_GAP / 15:   # trou : tenu seulement si l'objet est resté dans la même direction
                ang = math.degrees(math.acos(max(-1.0, min(1.0, float(np.dot(a[1:4], b[1:4]))))))
                if gap > HOLD_GAP_S or ang > HOLD_ANGLE:
                    continue
            d = np.array(a[1:4]) * (1 - f) + np.array(b[1:4]) * f
            ax, ay = a[4] * (1 - f) + b[4] * f, a[5] * (1 - f) + b[5] * f
        out.append((d / np.linalg.norm(d), ax, ay))
    return out


# ------------------------------------------------------------------ zones tracées à la main

MANUAL_WINDOW_S = 15.0    # suivi jusqu'à 15 s avant et après l'instant du tracé
MANUAL_SIZE = 400         # vue locale carrée centrée sur la zone
MANUAL_STEP = 2           # une image sur deux
MANUAL_GOOD_SCORE = 0.35   # position acceptée
MANUAL_MIN_SCORE = 0.15    # en dessous : suivi perdu
MANUAL_HOLD_S = 2.0        # zone tenue à sa dernière position sûre (passage devant, flou…)
MANUAL_MATCH = 0.55        # corrélation avec l'image d'origine de la zone : élément retrouvé


def local_view(d):
    """Vue (écran → caméra) regardant dans la direction d."""
    import geometry
    d = np.asarray(d, float) / np.linalg.norm(d)
    return geometry.view_matrix(math.degrees(math.atan2(d[0], d[2])), math.degrees(math.asin(max(-1.0, min(1.0, d[1])))))


def local_fov(ax, ay):
    """Champ de la vue locale : la zone en occupe ~1/5, entre 25 et 90°."""
    return max(25.0, min(90.0, math.degrees(2 * max(ax, ay)) * 5))


def tight_box(d, ax, ay, M, fov, size):
    """Boîte sans marge (sphere_to_box en ajoute une) : initialisation du suivi."""
    x, y, w, h = sphere_to_box(d, ax, ay, M, fov, size, size)
    cx, cy = x + w / 2, y + h / 2
    w, h = w / (1 + PAD), h / (1 + PAD)
    return int(cx - w / 2), int(cy - h / 2), max(4, int(w)), max(4, int(h))


def decode(h264, size, step):
    """Images BGR d'un rendu (une sur `step`)."""
    proc = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(h264), "-f", "rawvideo", "-pix_fmt", "bgr24", "-"],
                            stdout=subprocess.PIPE)
    frames, n, i = [], size * size * 3, 0
    while True:
        buf = proc.stdout.read(n)
        if len(buf) < n:
            break
        if i % step == 0:
            frames.append(np.frombuffer(buf, np.uint8).reshape(size, size, 3))
        i += 1
    proc.stdout.close()
    proc.wait()
    return frames


def follow(frames, k0, box, direction, fps=30000 / 1001):
    """Suit `box` depuis l'image k0 vers l'avant (+1) ou l'arrière (-1) ; {indice: boîte}.

    Score faible (occultation, flou de mouvement) : la zone reste à sa dernière position sûre
    au lieu de dériver avec le suiveur ; au-delà de MANUAL_HOLD_S sans position sûre, arrêt.
    """
    import cv2
    params = cv2.TrackerVit_Params()
    params.net = str(TRACK_MODEL)
    tracker = cv2.TrackerVit.create(params)
    tracker.init(frames[k0], box)
    out, k = {k0: box}, k0 + direction
    size = frames[k0].shape[0]
    x0, y0, w0, h0 = box
    template = frames[k0][y0:y0 + h0, x0:x0 + w0]
    last, weak = box, 0
    max_weak = int(MANUAL_HOLD_S * fps / MANUAL_STEP)

    def rematch(img, around):
        """Recherche de l'image d'origine de la zone autour de la dernière position sûre."""
        x, y, w, h = around
        m = max(w, h)   # fenêtre étroite : pendant une occultation l'élément bouge peu
        xa, ya = max(0, x - m), max(0, y - m)
        area = img[ya:min(size, y + h + m), xa:min(size, x + w + m)]
        if area.shape[0] <= h0 or area.shape[1] <= w0 or template.size == 0:
            return None
        res = cv2.matchTemplate(area, template, cv2.TM_CCOEFF_NORMED)
        _, best, _, loc = cv2.minMaxLoc(res)
        return (xa + loc[0], ya + loc[1], w0, h0) if best >= MANUAL_MATCH else None

    while 0 <= k < len(frames):
        ok, b = tracker.update(frames[k])
        score = tracker.getTrackingScore() if ok else 0.0
        x, y, w, h = b
        inside = not (x < 2 or y < 2 or x + w > size - 2 or y + h > size - 2)
        if ok and score >= MANUAL_GOOD_SCORE and inside:
            last, weak = tuple(int(v) for v in b), 0
        else:
            found = rematch(frames[k], last)
            if found:                      # élément retrouvé (fin d'occultation) : on repart de là
                last, weak = found, 0
                tracker = cv2.TrackerVit.create(params)
                tracker.init(frames[k], found)
            elif weak < max_weak:
                weak += 1
            else:
                break
        out[k] = last
        k += direction
    # fin tenue sans position sûre : on ne garde pas cette queue incertaine au-delà de la tenue
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

AUTO_NEIGHBORS = 2   # mode détection directe : zones des 2 images voisines ajoutées (pas de clignotement)


def _blur(img, boxes, W, H):
    import cv2
    for box in boxes:
        x0, y0 = int(max(0, box[0])), int(max(0, box[1]))
        x1, y1 = int(min(W, box[0] + box[2])), int(min(H, box[1] + box[3]))
        if x1 - x0 < 2 or y1 - y0 < 2:
            continue
        roi = img[y0:y1, x0:x1]
        k_size = max(3, (max(x1 - x0, y1 - y0) // 3) | 1)
        img[y0:y1, x0:x1] = cv2.GaussianBlur(cv2.GaussianBlur(roi, (k_size, k_size), 0), (k_size, k_size), 0)


def blur_video(src, dst, times, mats, fovs, tracks, W, H, encoder_args, detector=None, progress=None):
    """Floute les zones des pistes dans `src` (une entrée times/mats/fovs par image) → `dst`.

    Avec `detector` (résumé hyperlapse : images trop espacées pour un suivi), chaque image est
    aussi analysée et ses détections floutées, avec celles des images voisines.
    Retourne le nombre d'images modifiées. Le son est recopié tel quel.
    """
    import cv2
    from collections import deque
    rate = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate",
                           "-of", "csv=p=0", str(src)], capture_output=True, text=True).stdout.strip() or "30000/1001"
    dec = subprocess.Popen(["ffmpeg", "-v", "error", "-i", str(src), "-f", "rawvideo", "-pix_fmt", "bgr24", "-"],
                           stdout=subprocess.PIPE)
    enc = subprocess.Popen(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgr24", "-s", f"{W}x{H}",
                            "-r", rate, "-i", "-", "-i", str(src), "-map", "0:v", "-map", "1:a?",
                            *encoder_args, "-c:a", "copy", "-movflags", "+faststart", str(dst)],
                           stdin=subprocess.PIPE)
    size, touched = W * H * 3, 0
    scale = min(1.0, AW / W)   # détection sur une image ≤ 1920 px de large
    n = len(times)
    window = deque()           # (indice, image, boîtes détectées) : attente des voisines

    def detect(img):
        if detector is None:
            return []
        small = img if scale == 1.0 else cv2.resize(img, (int(W * scale), int(H * scale)))
        out = []
        for _, _, x, y, w, h in detector(small):
            cx, cy, hw, hh = (x + w / 2) / scale, (y + h / 2) / scale, w / scale * (1 + PAD) / 2, h / scale * (1 + PAD) / 2
            out.append((cx - hw, cy - hh, 2 * hw, 2 * hh))
        return out

    def emit(k, img):
        nonlocal touched
        j = min(k, n - 1)
        boxes = [b for d, ax, ay in regions_at(tracks, times[j])
                 if (b := sphere_to_box(d, ax, ay, mats[j], fovs[j], W, H)) is not None]
        boxes += [b for i, _, dets in window if abs(i - k) <= AUTO_NEIGHBORS for b in dets]
        if boxes:
            img = img.copy()
            _blur(img, boxes, W, H)
            touched += 1
        enc.stdin.write(img.tobytes())

    fi, head = 0, 0
    while True:
        buf = dec.stdout.read(size)
        if len(buf) < size:
            break
        img = np.frombuffer(buf, np.uint8).reshape(H, W, 3)
        window.append((fi, img, detect(img)))
        fi += 1
        while window and window[0][0] < fi - 2 * AUTO_NEIGHBORS - 1:
            window.popleft()
        # l'image « head » a toutes ses voisines suivantes : on l'écrit
        while head <= fi - 1 - AUTO_NEIGHBORS:
            emit(head, next(x[1] for x in window if x[0] == head))
            head += 1
        if progress and n:
            progress(min(1.0, fi / n))
    while head < fi:
        emit(head, next(x[1] for x in window if x[0] == head))
        head += 1
    dec.stdout.close()
    dec.wait()
    enc.stdin.close()
    if enc.wait() != 0:
        raise RuntimeError("floutage : échec de l'encodage")
    return touched
