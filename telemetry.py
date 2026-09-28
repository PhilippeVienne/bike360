"""Incrustation de télémétrie dans les exports : vitesse, mini-carte, profil d'altitude, lieu.

Les éléments fixes (panneaux, tracé, profil) sont dessinés une fois par morceau en PNG
(numpy → ffmpeg) ; ffmpeg les superpose et met à jour vitesse, altitude et position du
point 10 fois par seconde (sendcmd). Tailles proportionnelles à la hauteur de sortie.
"""
import json
import math
import subprocess
from datetime import datetime, timezone
from pathlib import Path

import numpy as np

import analyze

FONT_BOLD = "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf"
FONT = "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf"
ACCENT = (245, 165, 36)
UPDATE_HZ = 10
DEFAULTS = {"enabled": False, "speed": True, "map": True, "altitude": True, "place": True}


# ------------------------------------------------------------------ dessin (RGBA numpy)

def _canvas(w, h):
    return np.zeros((h, w, 4), np.float32)


def _rounded_panel(w, h, radius, alpha=0.45):
    img = _canvas(w, h)
    yy, xx = np.mgrid[0:h, 0:w] + 0.5
    dx = np.maximum(np.maximum(radius - xx, xx - (w - radius)), 0)
    dy = np.maximum(np.maximum(radius - yy, yy - (h - radius)), 0)
    inside = np.clip(radius + 0.5 - np.hypot(dx, dy), 0, 1)
    img[..., 3] = inside * alpha * 255
    return img


def _stroke(img, pts, width, color, alpha=1.0):
    """Trait anticrénelé le long d'une polyligne (pixels)."""
    h, w = img.shape[:2]
    r = width / 2
    for (x0, y0), (x1, y1) in zip(pts[:-1], pts[1:]):
        n = max(1, int(math.hypot(x1 - x0, y1 - y0) * 2))
        for k in range(n + 1):
            cx, cy = x0 + (x1 - x0) * k / n, y0 + (y1 - y0) * k / n
            xa, xb = int(max(0, cx - r - 1)), int(min(w, cx + r + 2))
            ya, yb = int(max(0, cy - r - 1)), int(min(h, cy + r + 2))
            if xa >= xb or ya >= yb:
                continue
            yy, xx = np.mgrid[ya:yb, xa:xb] + 0.5
            cov = np.clip(r + 0.5 - np.hypot(xx - cx, yy - cy), 0, 1) * alpha
            sub = img[ya:yb, xa:xb]
            a_new = np.maximum(sub[..., 3] / 255, cov)
            mix = cov > sub[..., 3] / 255
            for c in range(3):
                sub[..., c] = np.where(mix, color[c], sub[..., c])
            sub[..., 3] = a_new * 255


def _dot(size):
    img = _canvas(size, size)
    yy, xx = np.mgrid[0:size, 0:size] + 0.5
    d = np.hypot(xx - size / 2, yy - size / 2)
    ring = np.clip(size / 2 - d, 0, 1)
    core = np.clip(size / 2 - size * 0.18 - d, 0, 1)
    for c in range(3):
        img[..., c] = np.where(core > 0, 255, ACCENT[c])
    img[..., 3] = ring * 255
    return img


def _save_png(img, path):
    h, w = img.shape[:2]
    data = np.clip(img, 0, 255).astype(np.uint8).tobytes()
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", f"{w}x{h}",
                    "-i", "-", str(path)], input=data, check=True)


# ------------------------------------------------------------------ données

def _series(result, key):
    v = np.array([np.nan if x is None else x for x in result["series"][key]], float)
    ok = ~np.isnan(v)
    if ok.sum() < 2:
        return None
    idx = np.arange(len(v))
    return np.interp(idx, idx[ok], v[ok])


def place_at(result, t_session):
    """Commune (1er élément de l'adresse GeoRide) la plus proche dans le temps, ou ''."""
    day = datetime.fromtimestamp(result["utc_t0"], timezone.utc)
    path = analyze.DATA / f"georide_pos_{day:%Y%m%d}.json"
    if not path.exists():
        return ""
    pos = json.loads(path.read_text())
    target = result["utc_t0"] + result["offset_s"] + t_session
    best = min(pos, key=lambda p: abs(datetime.fromisoformat(p["fixtime"].replace("Z", "+00:00")).timestamp() - target),
               default=None)
    return ((best or {}).get("address") or "").split(",")[0]


def _escape(text):
    return text.replace("\\", "\\\\").replace("'", "’").replace(":", "\\:").replace("%", "\\%")


# ------------------------------------------------------------------ incrustation

def overlay(part, out, result, clip, t0, dur, W, H, opts, first_part, encoder_args, workdir, time_map=None):
    """Incruste la télémétrie sur `part` (morceau commençant à t0, temps de session) → `out`.

    `time_map(t_sortie) → t_session` (vectorisée) pour un temps non linéaire (hyperlapse) ;
    par défaut t0 + t.
    """
    opts = {**DEFAULTS, **(opts or {})}
    lat, lon = _series(result, "lat"), _series(result, "lon")
    speed, alt = _series(result, "speed"), _series(result, "alt")
    if lat is None or speed is None:
        return False
    workdir.mkdir(parents=True, exist_ok=True)
    U = min(W, H)                 # tailles relatives au petit côté (16:9, 1:1 ou 9:16)
    m = int(0.03 * U)
    # vertical (Reels, TikTok, Shorts) : l'interface de l'appli recouvre le haut et le bas
    top = int(0.09 * H) if H > W else m
    bottom = int(0.16 * H) if H > W else m
    inputs, chain, cmds = [], [], {}
    label = "0:v"

    def add_image(img, name):
        path = workdir / f"{name}.png"
        _save_png(img, path)
        inputs.extend(["-i", str(path)])
        return f"{len(inputs) // 2}:v"

    out_times = np.arange(0, dur + 1e-6, 1 / UPDATE_HZ)
    times = time_map(out_times) if time_map else t0 + out_times
    at = lambda arr, t: float(np.interp(t, np.arange(len(arr)), arr))

    # --- mini-carte + profil d'altitude (haut droite)
    S = int(0.26 * U)
    if opts["map"]:
        lat0 = math.radians(np.nanmean(lat))
        X, Y = np.radians(lon) * math.cos(lat0), np.radians(lat)
        span = max(np.ptp(X), np.ptp(Y)) or 1e-9
        pad = S * 0.1
        to_px = lambda i: (pad + (X[i] - X.min() + (span - np.ptp(X)) / 2) / span * (S - 2 * pad),
                           S - pad - (Y[i] - Y.min() + (span - np.ptp(Y)) / 2) / span * (S - 2 * pad))
        panel = _rounded_panel(S, S, S * 0.08)
        step = max(1, len(lat) // 800)
        _stroke(panel, [to_px(i) for i in range(0, len(lat), step)], max(2, U * 0.004), (200, 200, 200), 0.85)
        a, b = int(clip["start"]), int(min(len(lat) - 1, clip["end"]))
        _stroke(panel, [to_px(i) for i in range(a, b + 1)], max(3, U * 0.008), ACCENT)
        mx, my = W - m - S, top
        tag = add_image(panel, "map")
        chain.append(f"[{label}][{tag}]overlay={mx}:{my}[m1]")
        label = "m1"
        D = max(8, int(U * 0.024)) // 2 * 2
        dot = add_image(_dot(D), "dot")
        x0, y0 = to_px(int(t0))
        chain.append(f"[{label}][{dot}]overlay@dot={mx + x0 - D / 2:.1f}:{my + y0 - D / 2:.1f}[m2]")
        label = "m2"
        PX = np.array([to_px(i) for i in range(len(X))])
        idx = np.arange(len(X))
        cmds["dot"] = [(None, mx + np.interp(t, idx, PX[:, 0]) - D / 2, my + np.interp(t, idx, PX[:, 1]) - D / 2)
                       for t in times]
    if opts["altitude"] and alt is not None:
        PH = int(0.08 * U)
        py = top + (S + int(0.012 * U) if opts["map"] else 0)
        px_ = W - m - S
        prof = _rounded_panel(S, PH, PH * 0.2)
        lo, hi = np.nanmin(alt), np.nanmax(alt)
        n = len(alt)
        pts = [(S * 0.04 + i / (n - 1) * S * 0.92, PH * 0.85 - (alt[i] - lo) / max(hi - lo, 1) * PH * 0.55)
               for i in range(0, n, max(1, n // 400))]
        _stroke(prof, pts, max(2, U * 0.003), (220, 220, 220), 0.9)
        a, b = int(clip["start"]), int(min(n - 1, clip["end"]))
        _stroke(prof, [(S * 0.04 + i / (n - 1) * S * 0.92, PH * 0.85 - (alt[i] - lo) / max(hi - lo, 1) * PH * 0.55)
                       for i in range(a, b + 1, max(1, (b - a) // 100))], max(3, U * 0.005), ACCENT)
        tag = add_image(prof, "profile")
        chain.append(f"[{label}][{tag}]overlay={px_}:{py}[p1]")
        label = "p1"
        cur = _canvas(max(2, int(U * 0.003)), PH)
        cur[..., :3] = 255
        cur[..., 3] = 230
        ctag = add_image(cur, "cursor")
        cx = lambda t: px_ + S * 0.04 + min(1, max(0, t / (n - 1))) * S * 0.92
        chain.append(f"[{label}][{ctag}]overlay@cur={cx(t0):.1f}:{py}[p2]")
        label = "p2"
        cmds["cur"] = [(None, cx(t)) for t in times]
        fs = int(PH * 0.3)
        chain.append(f"[{label}]drawtext@alt=fontfile={FONT_BOLD}:text='{int(at(alt, t0))} m':fontsize={fs}"
                     f":fontcolor=white:x={px_ + int(S * 0.05)}:y={py + int(PH * 0.08)}[p3]")
        label = "p3"
        cmds["alt"] = [(None, f"{int(round(at(alt, t)))} m") for t in times]

    # --- vitesse (bas gauche)
    if opts["speed"]:
        BW, BH = int(0.22 * U), int(0.13 * U)
        tag = add_image(_rounded_panel(BW, BH, BH * 0.18), "speed")
        bx, by = m, H - bottom - BH
        chain.append(f"[{label}][{tag}]overlay={bx}:{by}[s1]")
        fs = int(BH * 0.62)
        chain.append(f"[s1]drawtext@spd=fontfile={FONT_BOLD}:text='{int(round(at(speed, t0)))}':fontsize={fs}"
                     f":fontcolor=white:x={bx + int(BW * 0.08)}:y={by + int(BH * 0.12)}[s2]")
        chain.append(f"[s2]drawtext=fontfile={FONT}:text='km/h':fontsize={int(BH * 0.22)}:fontcolor=white@0.85"
                     f":x={bx + int(BW * 0.66)}:y={by + int(BH * 0.55)}[s3]")
        label = "s3"
        cmds["spd"] = [(None, str(int(round(at(speed, t))))) for t in times]

    # --- lieu au début du clip (fondu)
    if opts["place"] and first_part:
        place = place_at(result, clip["start"])
        if place:
            fade = "if(lt(t,0.6),t/0.6,if(lt(t,3.4),1,max(0,(4-t)/0.6)))"
            chain.append(f"[{label}]drawtext=fontfile={FONT_BOLD}:text='{_escape(place)}':fontsize={int(0.05 * U)}"
                         f":fontcolor=white:alpha='{fade}':shadowcolor=black@0.6:shadowx=2:shadowy=2"
                         f":x={m}:y={top}:enable='lt(t,4)'[l1]")
            label = "l1"

    if not chain:
        return False
    # commandes de mise à jour
    lines = []
    for k in range(len(times)):
        parts = []
        if "dot" in cmds:
            _, x, y = cmds["dot"][k]
            parts += [f"overlay@dot x {x:.1f}", f"overlay@dot y {y:.1f}"]
        if "cur" in cmds:
            parts.append(f"overlay@cur x {cmds['cur'][k][1]:.1f}")
        if "alt" in cmds:
            parts.append(f"drawtext@alt reinit text={cmds['alt'][k][1].replace(' ', chr(0xA0))}")
        if "spd" in cmds:
            parts.append(f"drawtext@spd reinit text={cmds['spd'][k][1]}")
        if parts:
            lines.append(f"{out_times[k]:.2f} " + ", ".join(parts) + ";")
    cmdfile = workdir / "telemetry.cmd"
    cmdfile.write_text("\n".join(lines) + "\n")
    graph = f"[0:v]sendcmd=f='{cmdfile}'[v0];" + ";".join(chain).replace("[0:v]", "[v0]", 1) + f";[{label}]format=yuv420p[vout]"
    cmd = ["ffmpeg", "-v", "error", "-y", "-i", str(part), *inputs, "-filter_complex", graph,
           "-map", "[vout]", "-map", "0:a?", *encoder_args, "-c:a", "copy", "-movflags", "+faststart", str(out)]
    subprocess.run(cmd, check=True, capture_output=True, text=True)
    return True
