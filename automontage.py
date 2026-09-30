"""Montage automatique : les meilleurs moments des sessions du projet, pour une durée cible.

Sur la courbe d'intérêt de chaque session (analyze : virages, rotation, dénivelé, vitesse),
on retient des pics espacés d'au moins MIN_GAP_S ; chaque clip s'étend tant que l'intérêt
reste proche du pic (6 à 14 s). Les sessions reçoivent une part proportionnelle à leur
durée, pour ne pas tout prendre dans le même col. Les clips existants sont évités.
"""
import secrets

import numpy as np

MIN_GAP_S = 60
CLIP_MIN_S, CLIP_MAX_S = 6.0, 14.0
EDGE_S = 20            # ni tout début ni toute fin de session (démarrage, arrêt)
PEAK_MIN = 0.55        # intérêt minimal d'un pic (score classé 0..1)
KEEP_RATIO = 0.85      # le clip couvre la zone où l'intérêt reste ≥ 85 % du pic
MIN_SESSION_S = 120    # sessions plus courtes ignorées (essais, arrêts)
VIEW = {"yaw": 0.0, "pitch": -10.0, "fov": 100.0, "roll": 0.0, "horizon": "fixe"}


def _score(result):
    return np.array([0.0 if v is None else v for v in result["series"]["score"]], float)


def peaks(result):
    """Pics d'intérêt [(t, score)] espacés d'au moins MIN_GAP_S, du plus fort au plus faible."""
    s = _score(result)
    n = len(s)
    out = []
    for k in np.argsort(-s):
        if s[k] < PEAK_MIN:
            break
        if EDGE_S <= k < n - EDGE_S and all(abs(k - t) > MIN_GAP_S for t, _ in out):
            out.append((int(k), float(s[k])))
    return out


def window(result, t):
    """Clip autour du pic t : étendu tant que l'intérêt reste proche du pic."""
    s = _score(result)
    thr = KEEP_RATIO * s[t]
    a = b = t
    while a > t - CLIP_MAX_S / 2 and a > 0 and s[a - 1] >= thr:
        a -= 1
    while b < t + CLIP_MAX_S / 2 and b < len(s) - 1 and s[b + 1] >= thr:
        b += 1
    length = min(CLIP_MAX_S, max(CLIP_MIN_S, b - a))
    start = max(0.0, min(len(s) - length, (a + b) / 2 - length / 2))
    return round(start, 2), round(start + length, 2)


def plan(results, target_s, existing, transition_s=0.6):
    """Clips à ajouter {sid: [clip]} pour atteindre ~target_s de montage.

    `results` : {sid: analyse} des sessions du projet ; `existing` : {sid: [clips]} à éviter.
    """
    sessions = {sid: r for sid, r in results.items() if r["duration"] >= MIN_SESSION_S}
    if not sessions:
        return {}
    total = sum(r["duration"] for r in sessions.values())
    quota = {sid: target_s * r["duration"] / total for sid, r in sessions.items()}
    cands = sorted(((sc, sid, t) for sid, r in sessions.items() for t, sc in peaks(r)), reverse=True)
    chosen, used, length = {sid: [] for sid in sessions}, {sid: 0.0 for sid in sessions}, 0.0

    def free(sid, a, b):
        for c in existing.get(sid, []) + chosen[sid]:
            if a < c["end"] + 5 and b > c["start"] - 5:
                return False
        return True

    for relax in (1.3, 99):   # d'abord en respectant les parts de chaque session, puis sans
        for sc, sid, t in cands:
            if length >= target_s:
                break
            a, b = window(sessions[sid], t)
            if used[sid] >= quota[sid] * relax or not free(sid, a, b):
                continue
            chosen[sid].append({"id": secrets.token_hex(4), "start": a, "end": b, **VIEW, "auto": True})
            used[sid] += b - a
            length += (b - a) - (transition_s if length else 0)
    return {sid: sorted(c, key=lambda x: x["start"]) for sid, c in chosen.items() if c}
