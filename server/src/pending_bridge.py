# Pont provisoire entre le serveur Rust et les modules Python pas encore portés
# (telemetry, endcard, finishing, basemap, privacy). Lancé par server/src/pending.rs :
#   python3 -c <ce fichier> <fonction>     (PYTHONPATH = dossier du code Python)
# Entrée : arguments JSON sur l'entrée standard. Sortie standard : lignes « @job {…} »
# (état de la tâche), « @pid N » (processus lancé, pour l'annulation) et enfin
# « @result <json> ». Tout le reste (affichages des modules) est ignoré.
import json
import os
import sys
from pathlib import Path

import numpy as np

_out = sys.stdout
sys.stdout = sys.stderr   # les print() des modules ne se mélangent pas au protocole


def _default(o):
    if hasattr(o, "tolist"):
        return o.tolist()
    if isinstance(o, Path):
        return str(o)
    if isinstance(o, tuple):
        return list(o)
    return str(o)


def _emit(tag, payload):
    _out.write(f"@{tag} {payload}\n")
    _out.flush()


class Job(dict):
    """Dictionnaire de tâche dont chaque mise à jour est transmise au serveur Rust."""

    def __setitem__(self, k, v):
        super().__setitem__(k, v)
        if k == "proc":
            _emit("pid", v.pid)
        else:
            _emit("job", json.dumps({k: v}, default=_default))

    def update(self, *a, **kw):
        for k, v in dict(*a, **kw).items():
            self[k] = v

    def pop(self, k, *d):
        return super().pop(k, *d)


def _relocate():
    """Aligne les chemins des modules Python sur la racine du serveur Rust (INSTA_BUILD_ROOT)."""
    root = os.environ.get("INSTA_BUILD_ROOT")
    if not root:
        return
    import analyze
    root = Path(root).resolve()
    data, cache = root / "data", root / "data" / "cache"
    analyze.ROOT, analyze.DATA, analyze.CACHE, analyze.OVERRIDES = root, data, cache, data / "overrides.json"
    for name in ("privacy", "basemap", "musiclib", "server"):
        mod = sys.modules.get(name)
        if mod is None:
            continue
        if name == "privacy":
            mod.MODELS = cache / "models"
            mod.FACE_MODEL = mod.MODELS / Path(mod.FACE_MODEL).name
            mod.TRACK_MODEL = mod.MODELS / Path(mod.TRACK_MODEL).name
            mod.DATA, mod.THUMBS = data / "privacy", cache / "privacy"
        elif name == "basemap":
            mod.CACHE = cache / "tiles" / "osm"
        elif name == "musiclib":
            mod.CATALOG, mod.MUSIC = cache / "incompetech.json", data / "music"
        elif name == "server":
            mod.ROOT, mod.SELECTIONS, mod.EXPORTS = root, data / "selections", root / "exports"
            mod.SETTINGS, mod.SOURCES, mod.PROJECT = data / "settings.json", data / "sources.json", data / "project.json"
            mod.THUMBS, mod.MUSIC = cache / "thumbs", data / "music"


def _import(*names):
    mods = [__import__(n) for n in names]
    _relocate()
    return mods


def _server():
    """Module server.py, avec le moteur GPU et l'état NVENC fournis par le serveur Rust."""
    (server,) = _import("server")
    import privacy  # noqa: F401  (chemins relocalisés)
    _relocate()
    if os.environ.get("INSTA_RENDER_BIN"):
        server.RENDER_BIN = Path(os.environ["INSTA_RENDER_BIN"])
    server.state["nvenc"] = os.environ.get("INSTA_NVENC") == "1"
    return server


def _session(s):
    import insta360
    return insta360.Session(s["id"], s["date"], s["time"],
                            [insta360.Segment(x["index"], x.get("insv"), x.get("lrv"), x.get("offset", 0.0),
                                              x.get("duration", 0.0)) for x in s["segments"]], s.get("parts", []))


def _tracks(result, tracks):
    """Analyses des sessions du montage, avec l'objet `result` lui-même pour la session courante
    (telemetry._map_panel la reconnaît par identité : `r is not result`)."""
    if tracks is None:
        return None
    return [result if t.get("id") == result.get("id") else t for t in tracks]


def _mats(mats):
    return [np.array(m, float).reshape(3, 3) for m in mats]


# ------------------------------------------------------------------ fonctions

def minimap(a):
    (telemetry,) = _import("telemetry")
    from PIL import Image
    result, clip, size = a["result"], a["clip"], a["size"]
    panel, project, _ = telemetry._map_panel(result, clip, _tracks(result, a["tracks"]), size, size / 0.26)
    if clip["end"] > clip["start"]:
        lat, lon = telemetry._series(result, "lat"), telemetry._series(result, "lon")
        if lat is not None:
            D = max(8, int(size * 0.06)) // 2 * 2
            x, y = project(lat[int(clip["start"])], lon[int(clip["start"])])
            dot = telemetry._dot(D)
            x0, y0 = int(round(x - D / 2)), int(round(y - D / 2))
            if 0 <= x0 <= size - D and 0 <= y0 <= size - D:
                telemetry._over(panel[y0:y0 + D, x0:x0 + D], dot)
    Path(a["out"]).parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(np.clip(panel, 0, 255).astype(np.uint8), "RGBA").save(a["out"])
    return a["out"]


def telemetry_layers(a):
    (telemetry,) = _import("telemetry")
    tm = a.get("time_map")
    time_map = (lambda t, ot=np.array(tm[0]), pt=np.array(tm[1]): np.interp(t, ot, pt)) if tm else None
    return telemetry.layers(a["result"], a["clip"], a["t0"], a["n_frames"], a["fps"], a["W"], a["H"], a["opts"],
                            a["first_part"], Path(a["workdir"]), time_map=time_map, tracks=_tracks(a["result"], a.get("tracks")))


def telemetry_overlay(a):
    (telemetry,) = _import("telemetry")
    return bool(telemetry.overlay(Path(a["part"]), Path(a["out"]), a["result"], a["clip"], a["t0"], a["dur"],
                                  a["W"], a["H"], a["opts"], first_part=a["first_part"],
                                  encoder_args=a["encoder_args"], workdir=Path(a["workdir"]), tracks=_tracks(a["result"], a.get("tracks"))))


def endcard_render(a):
    (endcard,) = _import("endcard")
    endcard.render(a["results"], a["W"], a["H"], Path(a["path"]), a.get("title", ""), a.get("credits", []))
    return a["path"]


def finishing_finish(a):
    (finishing,) = _import("finishing")
    finishing.finish([Path(f) for f in a["clip_files"]], Path(a["final"]), a["style"], a["encoder_args"], a["W"], a["H"],
                     Path(a["music"]) if a.get("music") else None, a.get("audio_bitrate", "160k"),
                     end_card=Path(a["end_card"]) if a.get("end_card") else None, credits=a.get("credits", []))
    return a["final"]


def privacy_frame_boxes(a):
    (privacy,) = _import("privacy")
    return privacy.frame_boxes(a["times"], _mats(a["mats"]), a["fovs"], a["tracks"], a["W"], a["H"])


def privacy_blur_video(a):
    (privacy,) = _import("privacy")
    detector = privacy.Detector() if a.get("detect") else None
    job = Job()
    return privacy.blur_video(Path(a["src"]), Path(a["dst"]), a["times"], _mats(a["mats"]), a["fovs"], a["tracks"],
                              a["W"], a["H"], a["encoder_args"], detector,
                              lambda f: job.__setitem__("blur_progress", float(f)))


def _install(server, a):
    """Sessions (et horizons prêts) fournis par le serveur Rust → état du module server."""
    for sid, s in a.get("sessions", {}).items():
        server.state["sessions"][sid] = (_session(s["session"]), s["result"])
        if s.get("horizon"):
            server.state["horizon"][sid] = {"status": "done", "progress": 1.0, "data": s["horizon"]}


def privacy_analyze(a):
    server = _server()
    _install(server, a)
    job = Job()
    return list(server.analyze_privacy(job, [tuple(x) for x in a["items"]], a.get("force", False), a.get("label", "")))


def privacy_manual_zone(a):
    server = _server()
    _install(server, a)
    job = Job(state="running")
    server.state["jobs"]["privacy"] = job
    server.run_manual_zone(a["sid"], a["clip"], a["t0"], a["d0"], a["ax"], a["ay"], a["track_it"])
    return {k: v for k, v in job.items() if k != "proc"}


def follow_track(a):
    server = _server()
    job = Job()
    track = server.track_sphere(job, _session(a["session"]), a["result"], a["t0"], a["d0"], a["ax"], a["ay"],
                                a["a"], a["b"])
    return [[float(t), [float(x) for x in d]] for t, d in track.items()]


if __name__ == "__main__":
    fn = globals()[sys.argv[1]]
    args = json.loads(sys.stdin.read() or "null")
    _emit("result", json.dumps(fn(args), default=_default))
