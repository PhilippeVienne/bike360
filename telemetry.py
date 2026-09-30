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
import basemap

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


def _over(base, layer):
    """Compose un calque RGBA par-dessus `base` (sur place)."""
    a = layer[..., 3:4] / 255
    base[..., :3] = base[..., :3] * (1 - a) + layer[..., :3] * a
    base[..., 3] = np.maximum(base[..., 3], layer[..., 3])


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


def _raw(result, key):
    """Série brute (NaN là où le GPS manque : tunnels, pertes) pour tracer sans raccord."""
    return np.array([np.nan if x is None else x for x in result["series"][key]], float)


def _runs(xs, ys, step):
    """Polylignes continues (listes de points) en coupant aux NaN, sous-échantillonnées."""
    runs, cur = [], []
    for i in range(0, len(xs), step):
        if np.isnan(xs[i]) or np.isnan(ys[i]):
            if len(cur) > 1:
                runs.append(cur)
            cur = []
        else:
            cur.append((float(xs[i]), float(ys[i])))
    if len(cur) > 1:
        runs.append(cur)
    return runs


def _map_panel(result, clip, tracks, S, U):
    """Mini-carte : fond de carte (ou panneau sombre hors ligne), tracés de toutes les sessions
    du montage, session courante plus marquée, portion du clip en couleur.

    Retourne (image RGBA, project(lat, lon) → pixels, attribution requise).
    """
    lat_all = np.concatenate([_raw(r, "lat") for r in tracks])
    lon_all = np.concatenate([_raw(r, "lon") for r in tracks])
    radius = S * 0.08
    base = basemap.render(lat_all, lon_all, S)
    if base is not None:
        img, project = base
        panel = _canvas(S, S)
        panel[..., :3] = img
        panel[..., 3] = _rounded_panel(S, S, radius, alpha=1.0)[..., 3]
        # fond sombre : autres balades gris clair, balade courante blanche, clip en couleur ; liseré sombre
        styles = ((190, 196, 205), 0.7), ((255, 255, 255), 0.95), (10, 12, 16)
    else:  # hors ligne : ancien panneau sombre, même cadrage (équirectangulaire)
        ok = ~(np.isnan(lat_all) | np.isnan(lon_all))
        lat0 = math.radians(np.nanmean(lat_all[ok]))
        X, Y = np.radians(lon_all[ok]) * math.cos(lat0), np.radians(lat_all[ok])
        span = max(np.ptp(X), np.ptp(Y)) or 1e-9
        pad = S * 0.1

        def project(lat, lon):
            x = np.radians(np.asarray(lon)) * math.cos(lat0)
            y = np.radians(np.asarray(lat))
            return (pad + (x - X.min() + (span - np.ptp(X)) / 2) / span * (S - 2 * pad),
                    S - pad - (y - Y.min() + (span - np.ptp(Y)) / 2) / span * (S - 2 * pad))
        panel = _rounded_panel(S, S, radius)
        styles = ((170, 170, 170), 0.6), ((210, 210, 210), 0.9), (0, 0, 0)

    def layer(runs, width, color, alpha=1.0):
        lay = _canvas(S, S)
        for run in runs:
            _stroke(lay, run, width, color, alpha)
        _over(panel, lay)
        return lay

    (other_c, other_a), (cur_c, cur_a), outline = styles
    # tracés clairs cernés de sombre : lisibles sur le relief comme sur le panneau sombre
    for r in tracks:
        if r is not result:
            px, py = project(_raw(r, "lat"), _raw(r, "lon"))
            runs = _runs(px, py, max(1, len(px) // 600))
            layer(runs, max(4, U * 0.006), outline, 0.6)
            layer(runs, max(2, U * 0.003), other_c, other_a)
    px, py = project(_raw(result, "lat"), _raw(result, "lon"))
    runs = _runs(px, py, max(1, len(px) // 800))
    layer(runs, max(5, U * 0.009), outline, 0.7)
    layer(runs, max(3, U * 0.005), cur_c, cur_a)
    a, b = int(clip["start"]), int(min(len(px) - 1, clip["end"]))
    clip_runs = _runs(px[a:b + 1], py[a:b + 1], 1)
    layer(clip_runs, max(8, U * 0.016), (10, 12, 16), 0.8)      # liseré sombre sous la couleur du clip
    layer(clip_runs, max(4, U * 0.01), ACCENT)
    panel[..., 3] = np.minimum(panel[..., 3], _rounded_panel(S, S, radius, alpha=1.0)[..., 3])
    return panel, project, base is not None


# ------------------------------------------------------------------ incrustation

def overlay(part, out, result, clip, t0, dur, W, H, opts, first_part, encoder_args, workdir, time_map=None,
            tracks=None):
    """Incruste la télémétrie sur `part` (morceau commençant à t0, temps de session) → `out`.

    `time_map(t_sortie) → t_session` (vectorisée) pour un temps non linéaire (hyperlapse) ;
    par défaut t0 + t. `tracks` : analyses des sessions du montage (étendue de la mini-carte).
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
        panel, project, attribution = _map_panel(result, clip, tracks or [result], S, U)
        mx, my = W - m - S, top
        tag = add_image(panel, "map")
        chain.append(f"[{label}][{tag}]overlay={mx}:{my}[m1]")
        label = "m1"
        if attribution:
            fs = max(8, int(U * 0.011))
            lines = basemap.ATTRIBUTION
            for k, text in enumerate(lines):
                y = my + S - int(S * 0.04) - (len(lines) - k) * int(fs * 1.45)
                chain.append(f"[{label}]drawtext=fontfile={FONT}:text='{_escape(text)}':fontsize={fs}"
                             f":fontcolor=0xdddddd:box=1:boxcolor=black@0.5:boxborderw=2"
                             f":x={mx + S - int(S * 0.05)}-tw:y={y}[m1a{k}]")
                label = f"m1a{k}"
        D = max(8, int(U * 0.024)) // 2 * 2
        dot = add_image(_dot(D), "dot")
        PXx, PXy = project(lat, lon)
        idx = np.arange(len(PXx))
        dot_x = lambda t: mx + np.interp(t, idx, PXx) - D / 2
        dot_y = lambda t: my + np.interp(t, idx, PXy) - D / 2
        chain.append(f"[{label}][{dot}]overlay@dot={dot_x(t0):.1f}:{dot_y(t0):.1f}[m2]")
        label = "m2"
        cmds["dot"] = [(None, dot_x(t), dot_y(t)) for t in times]
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
