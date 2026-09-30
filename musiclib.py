"""Bibliothèque de musiques libres (CC BY 4.0) : catalogue Incompetech de Kevin MacLeod.

Le catalogue (titre, durée, tempo, ambiance, instruments) est mis en cache une semaine.
Une musique choisie est téléchargée dans data/music/ avec son crédit, affiché à la fin du
montage comme l'exige la licence (titre, auteur, source, licence).
"""
import json
import time
import urllib.parse
import urllib.request

import analyze

CATALOG_URL = "https://incompetech.com/music/royalty-free/pieces.json"
MP3_URL = "https://incompetech.com/music/royalty-free/mp3-royaltyfree/{}"
CATALOG = analyze.CACHE / "incompetech.json"
MUSIC = analyze.DATA / "music"
CREDITS = MUSIC / "credits.json"
USER_AGENT = "bike360/1.0 (outil personnel de montage)"
CATALOG_MAX_AGE = 7 * 86400
ARTIST = "Kevin MacLeod"
LICENSE = "CC BY 4.0"
LICENSE_URL = "creativecommons.org/licenses/by/4.0/"
# ambiances utiles pour des vidéos de moto (libellés d'Incompetech → français)
MOODS = {"Driving": "Entraînant", "Uplifting": "Enthousiaste", "Epic": "Épique", "Action": "Action",
         "Bright": "Lumineux", "Grooving": "Groove", "Relaxed": "Détendu", "Calming": "Calme",
         "Intense": "Intense", "Dark": "Sombre"}


def _get(url, timeout=30):
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def catalog():
    """Catalogue complet (liste de pièces), rafraîchi au plus une fois par semaine."""
    if not CATALOG.exists() or time.time() - CATALOG.stat().st_mtime > CATALOG_MAX_AGE:
        try:
            data = _get(CATALOG_URL)
            json.loads(data)
            CATALOG.parent.mkdir(parents=True, exist_ok=True)
            CATALOG.write_bytes(data)
        except Exception:
            if not CATALOG.exists():
                raise
    return json.loads(CATALOG.read_text())


def _seconds(length):
    try:
        h, m, s = (int(x) for x in length.split(":"))
        return h * 3600 + m * 60 + s
    except (AttributeError, ValueError):
        return 0


def search(query="", mood="", min_s=60, limit=60):
    """Pièces correspondant à une ambiance et/ou à des mots (titre, description, instruments)."""
    words = [w for w in query.lower().split() if w]
    out = []
    for p in catalog():
        feel = p.get("feel") or ""
        if mood and mood not in [f.strip() for f in feel.split(",")]:
            continue
        text = " ".join(str(p.get(k) or "") for k in ("title", "description", "instruments", "feel")).lower()
        if any(w not in text for w in words):
            continue
        seconds = _seconds(p.get("length"))
        if seconds < min_s:
            continue
        out.append({"title": p["title"], "filename": p["filename"], "seconds": seconds, "bpm": p.get("bpm"),
                    "uploaded": p.get("uploaded") or "",
                    "feel": ", ".join(MOODS.get(f.strip(), f.strip()) for f in feel.split(",") if f.strip()),
                    "description": p.get("description") or "", "instruments": p.get("instruments") or "",
                    "preview": MP3_URL.format(urllib.parse.quote(p["filename"]))})
    out.sort(key=lambda x: x["uploaded"], reverse=True)   # les plus récentes d'abord
    return out[:limit]


def credits():
    return json.loads(CREDITS.read_text()) if CREDITS.exists() else {}


def download(filename):
    """Télécharge une pièce du catalogue dans data/music/ et enregistre son crédit."""
    piece = next((p for p in catalog() if p["filename"] == filename), None)
    if piece is None:
        raise ValueError("pièce inconnue")
    MUSIC.mkdir(parents=True, exist_ok=True)
    target = MUSIC / piece["filename"]
    if not target.exists():
        data = _get(MP3_URL.format(urllib.parse.quote(piece["filename"])), timeout=120)
        target.write_bytes(data)
    c = credits()
    c[piece["filename"]] = {"title": piece["title"], "artist": ARTIST, "source": "incompetech.com",
                            "license": LICENSE, "license_url": LICENSE_URL}
    CREDITS.write_text(json.dumps(c, indent=1, ensure_ascii=False))
    return piece["filename"]


def credit_lines(filename):
    """Lignes de crédit d'une musique de la bibliothèque, ou [] (musique envoyée par toi)."""
    c = credits().get(filename)
    if not c:
        return []
    return [f"Musique : « {c['title']} » — {c['artist']} ({c['source']})",
            f"Licence {c['license']} · {c['license_url']}"]
