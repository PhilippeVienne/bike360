"""Rotations de vue communes au serveur, à l'analyse, au moteur GPU et au shader de l'UI.

Repère caméra : x droite, y haut, z avant (objectif avant du .lrv/.insv). Attention, ce
repère est indirect : ne pas y calculer de produits vectoriels « physiques » (gyroscope).
Un rayon d'écran d devient d_cam = L·Ry(yaw)·Rp(pitch)·d, où L redresse l'horizon.
"""
import numpy as np


def rot(axis, a):
    c, s = np.cos(np.radians(a)), np.sin(np.radians(a))
    return {"p": np.array([[1, 0, 0], [0, c, s], [0, -s, c]]),
            "y": np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]]),
            "r": np.array([[c, -s, 0], [s, c, 0], [0, 0, 1]])}[axis]


def tilt_matrix(tilt):
    """Inclinaison fixe de la caméra (déduite de la gravité moyenne), cf. analyze.mount_tilt."""
    tilt = tilt or {"pitch": 0, "roll": 0}
    return rot("p", tilt["pitch"]) @ rot("r", tilt["roll"])


def min_rotation(up):
    """Rotation minimale amenant l'axe y (haut de la vue) sur `up` (haut réel, repère caméra)."""
    u = np.asarray(up, float)
    u = u / np.linalg.norm(u)
    y = np.array([0.0, 1.0, 0.0])
    axis = np.cross(y, u)
    s, c = np.linalg.norm(axis), float(y @ u)
    if s < 1e-9:
        return np.eye(3)
    k = axis / s
    K = np.array([[0, -k[2], k[1]], [k[2], 0, -k[0]], [-k[1], k[0], 0]])
    return np.eye(3) + s * K + (1 - c) * K @ K


def view_matrix(yaw, pitch, level=None, roll=0.0):
    """Rotation écran → caméra ; `level` = matrice de redressement (None = aucun).

    `roll` : rotation manuelle de l'image autour de l'axe de visée (°, positif = sens horaire).
    """
    m = rot("y", yaw) @ rot("p", pitch) @ rot("r", roll)
    return m if level is None else level @ m


def clip_horizon_mode(clip):
    """Mode d'horizon d'un clip : 'auto' (image), 'fixe' (support) ou 'aucun'.

    Compatibilité : les anciens clips n'avaient qu'un booléen `level` (vrai → 'auto').
    """
    mode = clip.get("horizon")
    if mode in ("auto", "fixe", "aucun"):
        return mode
    return "auto" if clip.get("level") else "aucun"


# Courbes de transition entre deux points clés (mêmes noms et formules que ui/geometry.js).
# La courbe d'un point clé s'applique au segment qui le suit (comme l'app Insta360).
EASINGS = {
    "linear": lambda u: u,
    "ease_in_out": lambda u: u * u * (3 - 2 * u),
    "ease_in": lambda u: u * u,
    "ease_out": lambda u: 1 - (1 - u) ** 2,
    "quick": lambda u: u ** 3 * (u * (6 * u - 15) + 10),          # plus vif au milieu
    "delay": lambda u: 0.0 if u < 0.5 else ((u - 0.5) * 2) ** 2 * (3 - 4 * (u - 0.5)),  # attend puis bouge
    "cut": lambda u: 0.0 if u < 1 else 1.0,                          # coupe franche au point suivant
}


def _base_view(clip):
    return {"yaw": clip.get("yaw", 0.0), "pitch": clip.get("pitch", 0.0), "roll": clip.get("roll", 0.0),
            "fov": clip.get("fov", 100.0)}


def clip_keyframes(clip):
    """Points clés d'un clip, triés ; compatibilité avec les anciens `roll_keys`."""
    if clip.get("keyframes"):
        return sorted(clip["keyframes"], key=lambda k: k["t"])
    if clip.get("roll_keys"):
        b = _base_view(clip)
        return [{**b, "t": t, "roll": r, "curve": "linear"} for t, r in sorted(clip["roll_keys"])]
    return []


def clip_view_at(clip, t_rel):
    """Cadrage (yaw, pitch, roll, fov en °) à t_rel secondes du début du clip."""
    keys = clip_keyframes(clip)
    if not keys:
        return _base_view(clip)
    if t_rel <= keys[0]["t"]:
        return {k: keys[0][k] for k in ("yaw", "pitch", "roll", "fov")}
    if t_rel >= keys[-1]["t"]:
        return {k: keys[-1][k] for k in ("yaw", "pitch", "roll", "fov")}
    i = next(i for i, k in enumerate(keys) if k["t"] > t_rel)
    a, b = keys[i - 1], keys[i]
    u = EASINGS.get(a.get("curve", "linear"), EASINGS["linear"])((t_rel - a["t"]) / (b["t"] - a["t"]))
    dyaw = (b["yaw"] - a["yaw"] + 180) % 360 - 180                 # plus court chemin
    return {"yaw": (a["yaw"] + dyaw * u + 180) % 360 - 180,
            "pitch": a["pitch"] + (b["pitch"] - a["pitch"]) * u,
            "roll": a["roll"] + (b["roll"] - a["roll"]) * u,
            "fov": a["fov"] + (b["fov"] - a["fov"]) * u}


def v360_angles(m):
    """Décompose une rotation en angles ffmpeg v360 : v360 applique Ry(y)·Rp(p)·Rr(−roll)."""
    p2 = np.degrees(np.arcsin(np.clip(m[1, 2], -1, 1)))
    y2 = np.degrees(np.arctan2(m[0, 2], m[2, 2]))
    n = (rot("y", y2) @ rot("p", p2)).T @ m
    return y2, p2, -np.degrees(np.arctan2(n[1, 0], n[0, 0]))
