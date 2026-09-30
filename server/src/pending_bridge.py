# Pont provisoire entre le serveur Rust et le module Python pas encore porté (privacy,
# et le suivi d'un compagnon qui repose dessus). Lancé par server/src/pending.rs :
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


def _mats(mats):
    return [np.array(m, float).reshape(3, 3) for m in mats]


# ------------------------------------------------------------------ fonctions

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
