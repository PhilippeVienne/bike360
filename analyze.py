#!/usr/bin/env python3
"""Analyse des sessions Insta360 : profil seconde par seconde (IMU + GPS GeoRide),
synchronisation automatique et détection de moments candidats.

Usage : python3 analyze.py [DCIM] [--force]
Résultats dans data/cache/<session>.json.
"""
import argparse
import json
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

import numpy as np

import georide
import insta360

ROOT = Path(__file__).resolve().parent
DATA = ROOT / "data"
CACHE = DATA / "cache"
OVERRIDES = DATA / "overrides.json"
DEFAULT_DCIM = "/run/media/philippe/Insta360 X5/DCIM"

SYNC_SEARCH_S = 120      # plage de recherche du décalage horloge caméra ↔ GPS
SYNC_MIN_DURATION = 300  # en dessous, corrélation peu fiable : décalage du jour
GPS_MAX_GAP_S = 5        # au-delà, pas de position valide
KNOTS_TO_KMH = 1.852


def _smooth(x, n):
    n = min(n, len(x))
    if n <= 1:
        return x
    k = np.ones(n) / n
    return np.convolve(np.nan_to_num(x), k, "same")


def _rank(x):
    """Rang percentile 0..1 (robuste aux unités et aux valeurs aberrantes)."""
    x = np.nan_to_num(x)
    r = np.empty(len(x))
    r[np.argsort(x, kind="stable")] = np.linspace(0, 1, len(x))
    return r


def _ffprobe_format(path, entry):
    return subprocess.run(["ffprobe", "-v", "error", "-show_entries", f"format{entry}",
                           "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip()


def creation_utc(path):
    out = _ffprobe_format(path, "_tags=creation_time")
    return datetime.fromisoformat(out.replace("Z", "+00:00")).timestamp()


CACHE_VERSION = 5          # à incrémenter quand le calcul change (invalide le cache)


def imu_profile(session):
    """Remplit offset/durée des segments (contigus dans une session).

    Retourne (n secondes, vibration g, rotation brute, gravité moyenne dans le repère IMU).
    Le temps vidéo de chaque échantillon IMU vient du calage exact des métadonnées.
    """
    parts = []
    acc_sum, acc_n = np.zeros(3), 0
    offset = 0.0
    for seg in session.segments:
        tv, acc, gyr = insta360.read_imu(seg.lrv)
        seg.offset = offset
        seg.duration = float(_ffprobe_format(seg.lrv, "=duration"))
        ok = (tv >= 0) & (tv < seg.duration)
        parts.append((offset + tv[ok], np.linalg.norm(acc[ok], axis=1), np.linalg.norm(gyr[ok], axis=1)))
        acc_sum += acc[ok].sum(0)
        acc_n += int(ok.sum())
        offset += seg.duration
    last = session.segments[-1]
    n = int(np.ceil(last.offset + last.duration))
    cnt, s1, s2, g = (np.zeros(n) for _ in range(4))
    for t, a, gy in parts:
        sec = np.floor(t).astype(int)
        ok = (sec >= 0) & (sec < n)
        cnt += np.bincount(sec[ok], minlength=n)
        s1 += np.bincount(sec[ok], a[ok], n)
        s2 += np.bincount(sec[ok], a[ok] ** 2, n)
        g += np.bincount(sec[ok], gy[ok], n)
    c = np.maximum(cnt, 1)
    vib = np.sqrt(np.maximum(s2 / c - (s1 / c) ** 2, 0))
    return n, vib, g / c, acc_sum / max(acc_n, 1)


def mount_tilt(gravity):
    """Inclinaison fixe de la caméra (support/guidon) déduite de la gravité moyenne.

    Repère caméra (x droite, y haut, z avant, repère indirect) = (+a2, −a0, +a1) dans le
    repère IMU. Signe de x validé par symétrie : en vue ±90°, l'horizon doit être à la même
    hauteur des deux côtés (une première validation « à l'œil » avait retenu le mauvais signe).
    Retourne pitch/roll (°) tels que tilt_matrix(tilt)·y = haut réel.
    """
    g = gravity / np.linalg.norm(gravity)
    return {"pitch": round(float(-np.degrees(np.arcsin(np.clip(g[1], -1, 1)))), 2),
            "roll": round(float(np.degrees(np.arctan2(-g[2], -g[0]))), 2)}


def ride_stats(t, g, valid):
    """Statistiques de trajet de la session (GPS)."""
    out = {"duration_s": int(len(t))}
    if valid.mean() < 0.3:
        return out
    lat, lon = np.radians(g["lat"]), np.radians(g["lon"])
    ok = valid[1:] & valid[:-1]
    a = np.sin(np.diff(lat) / 2) ** 2 + np.cos(lat[1:]) * np.cos(lat[:-1]) * np.sin(np.diff(lon) / 2) ** 2
    step = 2 * 6371000 * np.arcsin(np.sqrt(np.nan_to_num(a)))
    step[~ok | (step > 80)] = 0  # sauts GPS
    speed = np.nan_to_num(g["speed"])
    moving = valid & (speed > 3)
    alt = _smooth(np.where(valid, np.nan_to_num(g["alt"]), np.nan_to_num(np.nanmedian(g["alt"]))), 30)
    dalt = np.diff(alt)[ok[:len(alt) - 1]] if len(alt) > 1 else np.zeros(0)
    out.update({
        "distance_km": round(float(step.sum()) / 1000, 1),
        "moving_s": int(moving.sum()),
        "avg_speed_kmh": round(float(speed[moving].mean()), 1) if moving.any() else 0,
        "max_speed_kmh": round(float(np.percentile(speed[valid], 99.5)), 1),
        "alt_min_m": int(np.nanmin(np.where(valid, g["alt"], np.nan))),
        "alt_max_m": int(np.nanmax(np.where(valid, g["alt"], np.nan))),
        "climb_m": int(dalt[dalt > 0].sum()), "descent_m": int(-dalt[dalt < 0].sum()),
    })
    return out


def load_positions(day_utc):
    """Positions GeoRide d'un jour UTC, en cache dans data/."""
    path = DATA / f"georide_pos_{day_utc:%Y%m%d}.json"
    if not path.exists():
        pos = georide.fetch_positions(f"{day_utc:%Y-%m-%d}", f"{day_utc + timedelta(days=1):%Y-%m-%d}")
        DATA.mkdir(exist_ok=True)
        path.write_text(json.dumps(pos))
    pos = json.loads(path.read_text())
    if not pos:
        return None
    t = np.array([datetime.fromisoformat(p["fixtime"].replace("Z", "+00:00")).timestamp() for p in pos])
    order = np.argsort(t)
    col = lambda k: np.array([float(p.get(k) or 0) for p in pos])[order]
    # GeoRide renvoie la vitesse en nœuds (vérifié contre la vitesse déduite des positions : ×1,83)
    return {"t": t[order], "lat": col("latitude"), "lon": col("longitude"), "speed": col("speed") * KNOTS_TO_KMH,
            "alt": col("altitude"), "heading": np.degrees(np.unwrap(np.radians(col("angle"))))}


def sample_gps(gps, times):
    idx = np.clip(np.searchsorted(gps["t"], times), 1, len(gps["t"]) - 1)
    gap = np.minimum(np.abs(gps["t"][idx] - times), np.abs(gps["t"][idx - 1] - times))
    valid = gap <= GPS_MAX_GAP_S
    out = {k: np.where(valid, np.interp(times, gps["t"], gps[k]), np.nan) for k in gps if k != "t"}
    return out, valid


def auto_offset(gps, t0, vib):
    """Décalage (s) maximisant la corrélation vitesse GPS ↔ vibrations IMU."""
    t = np.arange(len(vib))
    best = (-1.0, 0.0)
    for off in np.arange(-SYNC_SEARCH_S, SYNC_SEARCH_S + 0.5, 0.5):
        g, valid = sample_gps(gps, t0 + t + off)
        if valid.sum() < 0.5 * len(t):
            continue
        c = np.corrcoef(g["speed"][valid], vib[valid])[0, 1]
        if c > best[0]:
            best = (float(c), float(off))
    return best


def score_and_candidates(n, vib, gyro, gps_s, valid):
    has_gps = valid.mean() > 0.5
    if has_gps:
        speed = np.nan_to_num(gps_s["speed"])
        turn = np.abs(np.gradient(np.nan_to_num(gps_s["heading"])))
        turn[speed < 10] = 0
        turn = _smooth(np.minimum(turn, 45), 5)
        climb = np.abs(np.gradient(_smooth(np.nan_to_num(gps_s["alt"]), 30)))
        score = 0.45 * _rank(turn) + 0.25 * _rank(gyro) + 0.15 * _rank(climb) + 0.15 * _rank(speed)
        moving = speed > 5
    else:
        turn = climb = np.full(n, np.nan)
        score = 0.6 * _rank(gyro) + 0.4 * _rank(vib)
        moving = vib > np.percentile(vib, 15)
    score = _smooth(np.where(moving, score, 0), 15)
    cands = []
    for k in np.argsort(-score):
        if score[k] < 0.5 or len(cands) >= max(3, n // 300):
            break
        if all(abs(k - c) > 90 for c in cands):
            cands.append(int(k))
    return score, turn, climb, sorted(cands)


def _clean(a, nd):
    return [None if not np.isfinite(v) else round(float(v), nd) for v in a]


def analyze(session, overrides, refs, force=False):
    CACHE.mkdir(parents=True, exist_ok=True)
    out_path = CACHE / f"{session.id}.json"
    key = [[Path(s.lrv).name, Path(s.lrv).stat().st_size] for s in session.segments]
    override = overrides.get(session.id)
    if out_path.exists() and not force:
        cached = json.loads(out_path.read_text())
        if cached["key"] == key and cached.get("override") == override and cached.get("version") == CACHE_VERSION:
            return cached

    n, vib, gyro, gravity = imu_profile(session)
    t0 = creation_utc(session.segments[0].lrv)
    try:
        gps = load_positions(datetime.fromtimestamp(t0, timezone.utc).replace(hour=0, minute=0, second=0))
    except Exception as e:  # pas de réseau / identifiants : on continue sans GPS
        print(f"  GeoRide indisponible : {e}")
        gps = None

    corr = None
    if override is not None:
        offset, source = override, "manuel"
    elif gps is not None and n >= SYNC_MIN_DURATION:
        corr, offset = auto_offset(gps, t0, vib)
        source = "corrélation"
    elif refs:
        # L'horloge caméra peut être resynchronisée (app) en cours de journée :
        # on reprend le décalage de la session fiable la plus proche dans le temps.
        offset = min(refs, key=lambda r: abs(r[0] - t0))[1]
        source = "session voisine"
    else:
        offset, source = 0.0, "aucun"

    t = np.arange(n)
    if gps is not None:
        g, valid = sample_gps(gps, t0 + offset + t)
    else:
        g, valid = {k: np.full(n, np.nan) for k in ("lat", "lon", "speed", "alt", "heading")}, np.zeros(n, bool)
    score, turn, climb, cands = score_and_candidates(n, vib, gyro, g, valid)

    result = {
        "id": session.id, "date": session.date, "time": session.time, "key": key, "override": override,
        "utc_t0": t0, "offset_s": offset, "offset_source": source, "corr": corr,
        "duration": n, "gps_coverage": round(float(valid.mean()), 3),
        "segments": [{"index": s.index, "lrv": Path(s.lrv).name, "insv": Path(s.insv).name if s.insv else None,
                      "offset": round(s.offset, 3), "duration": round(s.duration, 3)} for s in session.segments],
        "series": {
            "speed": _clean(g["speed"], 1), "alt": _clean(g["alt"], 0),
            "lat": _clean(g["lat"], 6), "lon": _clean(g["lon"], 6),
            "turn": _clean(turn, 1), "climb": _clean(climb, 2),
            "vib": _clean(vib, 3), "gyro": _clean(gyro, 0), "score": _clean(score, 3),
        },
        "candidates": cands,
        "version": CACHE_VERSION,
        "tilt": mount_tilt(gravity),
        "stats": ride_stats(t, g, valid),
    }
    out_path.write_text(json.dumps(result))
    return result


def analyze_all(dcim, force=False):
    overrides = json.loads(OVERRIDES.read_text()) if OVERRIDES.exists() else {}
    sessions = insta360.scan(dcim)
    # Les longues sessions d'abord : leur décalage sert de référence aux clips courts.
    sessions.sort(key=lambda s: -sum(Path(x.lrv).stat().st_size for x in s.segments))
    refs, results = [], {}
    for s in sessions:
        print(f"• {s.id} ({len(s.segments)} segment(s))")
        r = analyze(s, overrides, refs, force)
        if r["offset_source"] in ("manuel", "corrélation") and (r["corr"] or 1) > 0.5:
            refs.append((r["utc_t0"], r["offset_s"]))
        print(f"  {r['duration'] / 60:.1f} min, GPS {r['gps_coverage']:.0%}, décalage {r['offset_s']:+.1f} s "
              f"({r['offset_source']}), {len(r['candidates'])} candidats")
        results[s.id] = r
    return results


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("dcim", nargs="?", default=DEFAULT_DCIM)
    p.add_argument("--force", action="store_true")
    a = p.parse_args()
    analyze_all(a.dcim, a.force)
