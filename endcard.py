"""Carte de fin d'un montage : carte des balades et statistiques cumulées (image PNG).

Les statistiques viennent de l'analyse (analyze.ride_stats) de chaque session du montage :
distances, temps de roulage et dénivelés additionnés ; vitesse et altitude maximales.
"""
from datetime import datetime, timezone

import numpy as np
from PIL import Image, ImageDraw, ImageFont

import basemap
from telemetry import ACCENT, FONT, FONT_BOLD

MONTHS = ("janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre",
          "novembre", "décembre")
DAYS = ("lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche")
BG = (17, 19, 23)


def _num(x):
    return f"{x:,.0f}".replace(",", " ")


def summary(results):
    """Statistiques cumulées des sessions (liste d'analyses)."""
    st = [r.get("stats") or {} for r in results]
    tot = lambda k: sum(s.get(k, 0) for s in st)
    top = lambda k: max((s[k] for s in st if k in s), default=None)
    days = sorted({datetime.fromtimestamp(r["utc_t0"], timezone.utc).astimezone().date() for r in results})
    return {"days": days, "distance_km": tot("distance_km"), "moving_s": tot("moving_s"), "climb_m": tot("climb_m"),
            "max_speed_kmh": top("max_speed_kmh"), "alt_max_m": top("alt_max_m")}


def _date_label(days):
    if not days:
        return ""
    a, b = days[0], days[-1]
    if a == b:
        return f"{DAYS[a.weekday()].capitalize()} {a.day} {MONTHS[a.month - 1]} {a.year}"
    if (a.month, a.year) == (b.month, b.year):
        return f"Du {a.day} au {b.day} {MONTHS[b.month - 1]} {b.year}"
    return f"Du {a.day} {MONTHS[a.month - 1]} au {b.day} {MONTHS[b.month - 1]} {b.year}"


def _map(results, size):
    """Carte carrée des tracés (fond topographique si disponible), ou None sans GPS."""
    tracks = []
    for r in results:
        lat = np.array([np.nan if x is None else x for x in r["series"]["lat"]], float)
        lon = np.array([np.nan if x is None else x for x in r["series"]["lon"]], float)
        if np.isfinite(lat).sum() > 10:
            tracks.append((lat, lon))
    if not tracks:
        return None
    base = basemap.render(np.concatenate([t[0] for t in tracks]), np.concatenate([t[1] for t in tracks]), size,
                          pad=0.08, max_fill=2.0)
    if base is None:
        return None
    img, project = base
    im = Image.fromarray(img).convert("RGB")
    d = ImageDraw.Draw(im)
    w = max(5, size // 80)
    ends = []
    for lat, lon in tracks:
        x, y = project(lat, lon)
        run = []
        for px, py in zip(x, y):
            if np.isfinite(px) and np.isfinite(py):
                run.append((float(px), float(py)))
            elif len(run) > 1:
                d.line(run, fill=(25, 25, 35), width=w + 4, joint="curve")
                d.line(run, fill=ACCENT, width=w, joint="curve")
                run = []
            else:
                run = []
        if len(run) > 1:
            d.line(run, fill=(25, 25, 35), width=w + 4, joint="curve")
            d.line(run, fill=ACCENT, width=w, joint="curve")
        pts = [(float(px), float(py)) for px, py in zip(x, y) if np.isfinite(px) and np.isfinite(py)]
        if pts:
            ends.append((pts[0], pts[-1]))
    # départ (vert) et arrivée (sombre, centre blanc) de chaque balade
    r = max(6, size // 60)
    for (sx, sy), (ex, ey) in ends:
        d.ellipse((sx - r, sy - r, sx + r, sy + r), fill=(46, 170, 90), outline=(255, 255, 255), width=max(2, r // 3))
        d.ellipse((ex - r, ey - r, ex + r, ey + r), fill=(25, 25, 35), outline=(255, 255, 255), width=max(2, r // 3))
        d.ellipse((ex - r / 3, ey - r / 3, ex + r / 3, ey + r / 3), fill=(255, 255, 255))
    fs = max(10, size // 55)
    font = ImageFont.truetype(FONT, fs)
    text = " · ".join(basemap.ATTRIBUTION)
    tw = d.textlength(text, font=font)
    d.rectangle((size - tw - 12, size - fs - 10, size, size), fill=(10, 12, 16))
    d.text((size - tw - 6, size - fs - 7), text, font=font, fill=(200, 204, 210))
    return im


def render(results, W, H, path, title=""):
    """Image W×H de la carte de fin → `path`."""
    s = summary(results)
    im = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(im)
    U = min(W, H)
    landscape = W > H * 1.2   # 16:9 : carte à gauche, chiffres à droite ; carré et vertical : l'un sous l'autre
    square = not landscape and H < W * 1.2
    msize = int(H * 0.78) if landscape else int(W * (0.52 if square else 0.82))
    k = 0.8 if square else 1.0 if landscape else 1.2   # taille du texte selon la place disponible
    mp = _map(results, msize)
    if landscape:
        mx, my = int(W * 0.06), (H - msize) // 2
        tx, ty = mx + msize + int(W * 0.05), int(H * 0.2)
    else:
        mx, my = (W - msize) // 2, int(H * (0.05 if square else 0.1))
        tx, ty = (W - msize) // 2 if not square else int(W * 0.24), my + msize + int(U * 0.05)
    if mp is not None:
        mask = Image.new("L", (msize, msize), 0)
        ImageDraw.Draw(mask).rounded_rectangle((0, 0, msize - 1, msize - 1), radius=msize // 25, fill=255)
        im.paste(mp, (mx, my), mask)
    big, mid, small = (ImageFont.truetype(f, int(U * r * k)) for f, r in ((FONT_BOLD, 0.075), (FONT_BOLD, 0.052), (FONT, 0.034)))
    y = ty
    if title:
        d.text((tx, y), title, font=big, fill=(255, 255, 255))
        y += int(U * 0.1 * k)
    d.text((tx, y), _date_label(s["days"]), font=small, fill=(180, 186, 196))
    y += int(U * 0.075 * k)
    h, m = divmod(int(s["moving_s"]) // 60, 60)
    lines = []
    if s["distance_km"]:
        lines.append((f"{s['distance_km']:.0f} km".replace(".", ","), "parcourus"))
    if s["moving_s"]:
        lines.append((f"{h} h {m:02d}" if h else f"{m} min", "de roulage"))
    if s["climb_m"]:
        lines.append((f"{_num(s['climb_m'])} m", "de dénivelé positif"))
    if s["alt_max_m"]:
        lines.append((f"{_num(s['alt_max_m'])} m", "point culminant"))
    if s["max_speed_kmh"]:
        lines.append((f"{s['max_speed_kmh']:.0f} km/h", "vitesse max"))
    for value, label in lines:
        d.text((tx, y), value, font=mid, fill=ACCENT)
        vw = d.textlength(value + "  ", font=mid)
        d.text((tx + vw, y + int(U * 0.016 * k)), label, font=small, fill=(220, 224, 230))
        y += int(U * 0.075 * k)
    im.save(path)
    return s
