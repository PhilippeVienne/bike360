"""Fond de carte raster sombre pour les cartes des exports (tuiles OpenStreetMap, inversées).

Les tuiles sont téléchargées à la demande et gardées dans data/cache/tiles ; l'image doit
porter l'attribution ATTRIBUTION. Projection Web Mercator, comme les tuiles.
"""
import io
import math
import urllib.request
from concurrent.futures import ThreadPoolExecutor

import numpy as np
from PIL import Image

import analyze

TILE_URL = "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
TILE = 256
MAX_ZOOM = 16
UPSCALE = 1.4         # carte agrandie : noms de lieux lisibles une fois incrustés dans la vidéo
CACHE = analyze.CACHE / "tiles" / "osm"
ATTRIBUTION = ("© contributeurs OpenStreetMap",)  # une ligne chacune (mini-carte étroite)
USER_AGENT = "bike360/1.0 (outil personnel de montage)"


def stylize(img, saturation=0.4, brightness=0.72):
    """Fond sombre et sobre : carte OSM inversée, teinte retournée (l'eau reste bleue, les
    forêts vert sombre), couleurs atténuées. Routes et noms deviennent clairs sur fond sombre."""
    inv = 255 - img.astype(np.float32)
    g = inv @ np.array([0.299, 0.587, 0.114], np.float32)
    f = g[..., None] - (inv - g[..., None]) * saturation
    return np.clip(f * brightness + 8, 0, 255).astype(np.uint8)


def _world(lat, lon, z):
    """Coordonnées pixel (monde) Web Mercator au zoom z."""
    n = TILE * 2 ** z
    x = (np.asarray(lon) + 180) / 360 * n
    s = np.sin(np.radians(np.clip(lat, -85, 85)))
    y = (0.5 - np.log((1 + s) / (1 - s)) / (4 * math.pi)) * n
    return x, y


def _tile(z, x, y):
    path = CACHE / str(z) / str(x) / f"{y}.png"
    if not path.exists():
        url = TILE_URL.format(z=z, x=x, y=y)
        req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req, timeout=20) as r:
            data = r.read()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    im = Image.open(io.BytesIO(path.read_bytes())).convert("RGBA")
    bg = Image.new("RGBA", im.size, (242, 239, 233, 255))   # transparence éventuelle → fond clair
    return Image.alpha_composite(bg, im).convert("RGB")


def render(lats, lons, out_size, pad=0.12, max_fill=1.0):
    """Carte carrée de `out_size` px englobant les points (lat, lon) ; None si tuiles indisponibles.

    Retourne (image RGB uint8 out_size², project(lat, lon) → (x, y) pixels dans l'image).
    `max_fill` > 1 : agrandit encore jusqu'à ce facteur pour que les tracés remplissent la carte
    (le zoom des tuiles est entier : sans cela la carte peut être deux fois trop large).
    """
    size = int(round(out_size / UPSCALE))
    lats, lons = np.asarray(lats, float), np.asarray(lons, float)
    ok = ~(np.isnan(lats) | np.isnan(lons))
    if ok.sum() < 2:
        return None
    lats, lons = lats[ok], lons[ok]
    usable = size * (1 - 2 * pad)
    z = MAX_ZOOM
    while z > 1:
        x, y = _world(lats, lons, z)
        if max(np.ptp(x), np.ptp(y)) <= usable:
            break
        z -= 1
    x, y = _world(lats, lons, z)
    fill = max(1.0, min(max_fill, usable / max(np.ptp(x), np.ptp(y), 1e-9)))
    size = max(16, int(round(size / fill)))
    cx, cy = (x.min() + x.max()) / 2, (y.min() + y.max()) / 2
    x0, y0 = cx - size / 2, cy - size / 2
    tx0, ty0 = int(x0 // TILE), int(y0 // TILE)
    tx1, ty1 = int((x0 + size) // TILE), int((y0 + size) // TILE)
    keys = [(tx, ty) for ty in range(ty0, ty1 + 1) for tx in range(tx0, tx1 + 1)]
    try:
        with ThreadPoolExecutor(4) as pool:
            tiles = dict(zip(keys, pool.map(lambda k: _tile(z, k[0] % 2 ** z, k[1]), keys)))
    except Exception as e:  # hors ligne, service indisponible : mini-carte simple
        print(f"  fond de carte indisponible : {e}")
        return None
    mosaic = Image.new("RGB", ((tx1 - tx0 + 1) * TILE, (ty1 - ty0 + 1) * TILE))
    for (tx, ty), im in tiles.items():
        mosaic.paste(im, ((tx - tx0) * TILE, (ty - ty0) * TILE))
    ox, oy = int(round(x0 - tx0 * TILE)), int(round(y0 - ty0 * TILE))
    crop = mosaic.crop((ox, oy, ox + size, oy + size)).resize((out_size, out_size), Image.LANCZOS)
    img = stylize(np.asarray(crop))
    k = out_size / size

    def project(lat, lon):
        px, py = _world(lat, lon, z)
        return (px - (tx0 * TILE + ox)) * k, (py - (ty0 * TILE + oy)) * k
    return img, project
