"""Horizon mesuré dans l'image : la verticale est le point de fuite des contours verticaux.

Toutes les lignes verticales du monde (troncs, poteaux, murs, arêtes) passent par le
zénith sur la sphère : chaque contour définit un grand cercle de normale n et le « haut »
réel u vérifie n·u = 0.

Estimation robuste en trois parties :
1. image : score de chaque orientation candidate (grille roulis × tangage autour de
   l'inclinaison fixe de la caméra) = somme pondérée des contours compatibles ;
2. a priori physique : roulis centré sur l'inclinaison déduite du GPS, d'autant plus
   serré que la vitesse est élevée (les grands angles de guidon n'arrivent qu'au pas) ;
3. continuité : chemin le plus probable par Viterbi (pas de sauts isolés).

Ce calcul remplace l'IMU : avec une caméra au guidon, le gyroscope n'a pas pu être
exploité de façon fiable (voir historique). Pièges connus : marquages de route, glissières
et tunnels (lignes fuyantes), d'où l'a priori de vitesse (vitesse interpolée quand le GPS
décroche, ex. tunnel).
"""
import json
import os
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path

import numpy as np

import analyze
import geometry

HZ = 10                  # estimations par seconde de vidéo
W, H = 1024, 512         # image équirectangulaire d'analyse
MAX_EDGES = 3000         # contours retenus par image (les plus contrastés)
ROLLS = np.arange(-44, 45, 2.0)      # roulis résiduel candidat (°) par rapport à l'inclinaison fixe
PITCHES = np.arange(-18, 19, 3.0)    # tangage résiduel candidat (°)
EMISSION_WEIGHT = 4.0
TRANSITION_SIGMA = 3.0   # ° par pas de 0,1 s
CACHE_VERSION = 5          # 5 : signe du roulis de l'inclinaison fixe corrigé
RENDER_BIN = Path(__file__).resolve().parent / "render" / "target" / "release" / "insta-render"
EDGE_FILTER = {"lat_min": -15.0, "lat_max": 55.0, "excl_lon": [125.0, 235.0], "grad_min": 12.0, "weight_cap": 60.0}


def up_from_angles(roll_deg, pitch_deg):
    """Haut réel (repère de l'image équirect / caméra redressée de l'inclinaison fixe)."""
    R, P = np.radians(roll_deg), np.radians(pitch_deg)
    return np.stack([-np.sin(R) * np.cos(P), np.cos(R) * np.cos(P), np.sin(P)], -1)


_R, _P = np.meshgrid(ROLLS, PITCHES, indexing="ij")
STATES = up_from_angles(_R, _P).reshape(-1, 3)
# Le repère x droite / y haut / z avant est indirect : pour la géométrie des contours,
# le haut s'exprime en miroir x/z (validé visuellement sur des scènes de référence).
GEOMETRIC = (STATES * np.array([-1, 1, -1])).astype(np.float32)


def edge_normals(img, exclude=None):
    """Normales des grands cercles portés par les contours (hors moto en bas et pilote à l'arrière).

    `exclude` : masque (H, W) des pixels fixes par rapport à la caméra (guidon, rétroviseurs…),
    dont les contours voteraient pour « aucune correction ».
    """
    gx = np.zeros_like(img)
    gy = np.zeros_like(img)
    gx[:, 1:-1] = (img[:, 2:] - img[:, :-2]) / 2
    gy[1:-1] = (img[2:] - img[:-2]) / 2
    mag = np.hypot(gx, gy)
    lon = (np.arange(W) + 0.5) / W * 2 * np.pi - np.pi
    lat = np.pi / 2 - (np.arange(H) + 0.5) / H * np.pi
    LON, LAT = np.meshgrid(lon, lat)
    lon_deg = (np.degrees(LON) + 360) % 360
    keep = (mag > 12) & (np.degrees(LAT) > -15) & (np.degrees(LAT) < 55) & ~((lon_deg > 125) & (lon_deg < 235))
    keep[::2] = False
    keep[:, ::2] = False
    if exclude is not None:
        keep &= ~exclude
    ln, lt, ax, ay, w = LON[keep], LAT[keep], gx[keep], gy[keep], mag[keep]
    if len(w) > MAX_EDGES:
        sel = np.argpartition(-w, MAX_EDGES)[:MAX_EDGES]
        ln, lt, ax, ay, w = ln[sel], lt[sel], ax[sel], ay[sel], w[sel]
    d = np.stack([np.cos(lt) * np.sin(ln), np.sin(lt), np.cos(lt) * np.cos(ln)], 1)
    e_lon = np.stack([np.cos(ln), np.zeros_like(ln), -np.sin(ln)], 1)
    e_lat = np.stack([-np.sin(lt) * np.sin(ln), np.cos(lt), -np.sin(lt) * np.cos(ln)], 1)
    t = (-ay)[:, None] * e_lon - ax[:, None] * e_lat     # contour ⟂ gradient ; y image vers le bas
    n = np.cross(d, t)
    n /= np.linalg.norm(n, axis=1, keepdims=True) + 1e-9
    return n.astype(np.float32), np.minimum(w, 60).astype(np.float32)


def emission(img, sigma=0.04, exclude=None):
    """Log-score (≤ 0) de chaque état : poids des contours compatibles avec ce « haut »."""
    n, w = edge_normals(img, exclude)
    if len(w) == 0:
        return np.zeros(len(STATES), np.float32)
    s = (np.exp(-((GEOMETRIC @ n.T) / sigma) ** 2) * w).sum(1)
    return np.log(s / s.max() + 1e-6).astype(np.float32)


def speed_prior(speed_kmh, roll_center_deg):
    """Log-a priori (T, S) : roulis autour du centre donné, écart permis décroissant avec la vitesse."""
    sig = 8 + 25 * np.exp(-np.asarray(speed_kmh) / 30.0)
    return (-((_R.ravel()[None, :] - np.asarray(roll_center_deg)[:, None]) ** 2) / (2 * sig[:, None] ** 2)
            - _P.ravel()[None, :] ** 2 / (2 * 10.0 ** 2)).astype(np.float32)


def viterbi(E, prior):
    """Chemin d'états le plus probable ; transitions vers les états voisins (±6° roulis, ±6° tangage)."""
    nr, npi = len(ROLLS), len(PITCHES)
    T = len(E)
    score = EMISSION_WEIGHT * E[0] + prior[0]
    back = np.zeros((T, nr * npi), np.int32)
    ridx, pidx = np.meshgrid(np.arange(nr), np.arange(npi), indexing="ij")
    moves = []
    for a in range(-3, 4):
        for b in range(-2, 3):
            src_r, src_p = ridx - a, pidx - b
            valid = (src_r >= 0) & (src_r < nr) & (src_p >= 0) & (src_p < npi)
            pen = -((a * 2.0) ** 2 + (b * 3.0) ** 2) / (2 * TRANSITION_SIGMA ** 2)
            src = np.where(valid, src_r * npi + src_p, 0).ravel()
            moves.append((valid.ravel(), src, pen))
    for t in range(1, T):
        best = np.full(nr * npi, -np.inf, np.float32)
        arg = np.zeros(nr * npi, np.int32)
        for valid, src, pen in moves:
            cand = np.where(valid, score[src] + pen, -np.inf)
            m = cand > best
            best[m] = cand[m]
            arg[m] = src[m]
        back[t] = arg
        best = best.reshape(-1)
        score = best.ravel() + EMISSION_WEIGHT * E[t] + prior[t]
    path = np.empty(T, np.int32)
    path[-1] = int(np.argmax(score))
    for t in range(T - 1, 0, -1):
        path[t - 1] = back[t][path[t]]
    return path


def _gauss(x, sig):
    k = np.arange(-int(4 * sig), int(4 * sig) + 1)
    w = np.exp(-k ** 2 / (2 * sig ** 2))
    return np.convolve(np.pad(x, len(k) // 2, mode="edge"), w / w.sum(), "valid")


def gps_prior_inputs(result, n_total):
    """Vitesse (km/h, pertes GPS comblées) et inclinaison GPS (°) à HZ sur la session."""
    t = np.arange(n_total) / HZ
    try:
        gps = analyze.load_positions(datetime.fromtimestamp(result["utc_t0"], timezone.utc)
                                     .replace(hour=0, minute=0, second=0))
    except Exception:
        gps = None
    if gps is None:
        return np.full(n_total, 30.0), np.zeros(n_total)
    g, valid = analyze.sample_gps(gps, result["utc_t0"] + result["offset_s"] + t)
    if valid.sum() < 2:
        return np.full(n_total, 30.0), np.zeros(n_total)
    speed = np.interp(t, t[valid], g["speed"][valid])          # tunnels : interpolation
    heading = np.interp(t, t[valid], g["heading"][valid])
    yaw_rate = _gauss(np.gradient(heading) * HZ, 15)
    lean = np.degrees(np.arctan(speed / 3.6 * np.radians(yaw_rate) / 9.81))
    return speed, lean


def emissions_gpu(lrv, start, duration, base, progress=None):
    """Émissions calculées par le moteur CUDA (render/) ; None si indisponible ou en échec."""
    if not RENDER_BIN.exists():
        return None
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "scores.f32")
        spec = os.path.join(tmp, "job.json")
        Path(spec).write_text(json.dumps({
            "source": lrv, "start": start, "duration": duration, "hz": HZ,
            "base": [float(v) for v in np.asarray(base).flatten()],
            "states": GEOMETRIC.tolist(), "sigma": 0.04, "width": W, "height": H, **EDGE_FILTER,
            "output": out,
        }))
        proc = subprocess.Popen([str(RENDER_BIN), "horizon", spec], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        n_out = int(duration * HZ)
        for line in proc.stdout:
            if progress and line.startswith("frame="):
                progress(min(1.0, int(line[6:]) / max(n_out, 1)))
        if proc.wait() != 0:
            return None
        s = np.fromfile(out, "<f4").reshape(-1, len(STATES))
    top = s.max(1, keepdims=True)
    return np.where(top > 0, np.log(s / np.maximum(top, 1e-12) + 1e-6), 0.0).astype(np.float32)


def segment_emissions(lrv, duration, base, progress=None):
    gpu = emissions_gpu(lrv, 0.0, duration, base, progress)
    if gpu is not None:
        return gpu
    yaw, pitch, roll = geometry.v360_angles(base)
    vf = (f"fps={HZ},v360=input=dfisheye:ih_fov=195:iv_fov=195:output=equirect"
          f":yaw={yaw:.3f}:pitch={pitch:.3f}:roll={roll:.3f}:w={W}:h={H},format=gray")
    proc = subprocess.Popen(["ffmpeg", "-v", "error", "-i", lrv, "-vf", vf, "-f", "rawvideo", "-"],
                            stdout=subprocess.PIPE)
    out = []
    while True:
        buf = proc.stdout.read(W * H)
        if len(buf) < W * H:
            break
        out.append(emission(np.frombuffer(buf, np.uint8).reshape(H, W).astype(np.float32)))
        if progress and len(out) % HZ == 0:
            progress(min(1.0, len(out) / HZ / duration))
    proc.wait()
    return np.array(out, np.float32).reshape(-1, len(STATES))


def compute(session, result, cache_dir, progress=None):
    """Horizon d'une session entière (temps de session, HZ), mis en cache."""
    path = cache_dir / f"{session.id}_horizon.json"
    key = [result["key"], result.get("offset_s"), CACHE_VERSION]
    if path.exists():
        cached = json.loads(path.read_text())
        if cached.get("key") == key:
            return cached
    base = geometry.tilt_matrix(result.get("tilt"))
    total = sum(s["duration"] for s in result["segments"])
    n_total = int(np.ceil(total * HZ))
    E = np.zeros((n_total, len(STATES)), np.float32)
    done = 0.0
    for seg, info in zip(session.segments, result["segments"]):
        def seg_progress(f, b=done, d=info["duration"]):
            if progress:
                progress(0.95 * (b + f * d) / total)
        e = segment_emissions(seg.lrv, info["duration"], base, seg_progress)
        k0 = int(round(info["offset"] * HZ))
        k1 = min(n_total, k0 + len(e))
        E[k0:k1] = e[:k1 - k0]
        done += info["duration"]
    speed, lean = gps_prior_inputs(result, n_total)
    states = viterbi(E, speed_prior(speed, -lean))
    roll = _gauss(ROLLS[states // len(PITCHES)], 1.5)       # efface les paliers de la grille
    pitch = _gauss(PITCHES[states % len(PITCHES)], 1.5)
    up = (base @ up_from_angles(roll, pitch).T).T            # repère caméra
    # Pas d'indicateur de fiabilité : le contraste des scores ne distingue pas les scènes
    # faciles des difficiles (tunnel ≈ virages), il serait trompeur.
    data = {"key": key, "hz": HZ, "reliable": None, "up": np.round(up, 4).tolist()}
    path.write_text(json.dumps(data))
    if progress:
        progress(1.0)
    return data


def compute_range(session, result, t_start, t_end):
    """Horizon d'une portion de session seulement (export sans attendre l'analyse complète)."""
    base = geometry.tilt_matrix(result.get("tilt"))
    t_start = max(0.0, t_start)
    E = []
    for seg, info in zip(session.segments, result["segments"]):
        a = max(t_start, info["offset"])
        b = min(t_end, info["offset"] + info["duration"])
        if b - a <= 0:
            continue
        gpu = emissions_gpu(seg.lrv, a - info["offset"], b - a, base)
        if gpu is not None:
            E.extend(gpu)
            continue
        yaw, pitch, roll = geometry.v360_angles(base)
        vf = (f"fps={HZ},v360=input=dfisheye:ih_fov=195:iv_fov=195:output=equirect"
              f":yaw={yaw:.3f}:pitch={pitch:.3f}:roll={roll:.3f}:w={W}:h={H},format=gray")
        proc = subprocess.Popen(["ffmpeg", "-v", "error", "-ss", f"{a - info['offset']:.3f}", "-t", f"{b - a:.3f}",
                                 "-i", seg.lrv, "-vf", vf, "-f", "rawvideo", "-"], stdout=subprocess.PIPE)
        while (buf := proc.stdout.read(W * H)) and len(buf) == W * H:
            E.append(emission(np.frombuffer(buf, np.uint8).reshape(H, W).astype(np.float32)))
        proc.wait()
    if len(E) < 2:
        return None
    E = np.array(E, np.float32)
    n_total = int(np.ceil(t_start * HZ)) + len(E)
    speed, lean = gps_prior_inputs(result, n_total)
    k0 = n_total - len(E)
    states = viterbi(E, speed_prior(speed[k0:], -lean[k0:]))
    roll = _gauss(ROLLS[states // len(PITCHES)], 1.5)
    pitch = _gauss(PITCHES[states % len(PITCHES)], 1.5)
    up = (base @ up_from_angles(roll, pitch).T).T
    return {"hz": HZ, "t0": k0 / HZ, "up": np.round(up, 4).tolist()}


def level_at(data, t):
    """Matrice de redressement à l'instant t (temps de session), ou None."""
    if not data or not data.get("up"):
        return None
    up = np.asarray(data["up"])
    x = np.clip((t - data.get("t0", 0.0)) * data["hz"], 0, len(up) - 1)
    i = int(np.floor(x))
    j = min(i + 1, len(up) - 1)
    u = up[i] * (1 - (x - i)) + up[j] * (x - i)
    return geometry.min_rotation(u)
