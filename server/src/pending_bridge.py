# Pont provisoire entre le serveur Rust et le module Python pas encore porté (privacy, et le
# suivi d'un compagnon qui repose dessus). Lancé par server/src/pending.rs :
#   python3 -c <ce fichier> <fonction>     (PYTHONPATH = dossier du code Python)
# Entrée : arguments JSON sur l'entrée standard. Sortie standard : lignes « @job {…} »
# (état de la tâche), « @pid N » (processus lancé, pour l'annulation) et enfin
# « @result <json> ». Tout le reste (affichages des modules) part sur la sortie d'erreur.
#
# Les fonctions d'orchestration de server.py qu'il faut encore (analyse de confidentialité,
# zone tracée, suivi) sont recopiées ici : le pont ne dépend que de privacy.py et des
# modules qu'il importe (analyze, geometry, horizon), pas de server.py.
import json
import math
import os
import secrets
import subprocess
import sys
from fractions import Fraction
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


def _relocate():
    """Aligne les chemins des modules Python sur la racine du serveur Rust (INSTA_BUILD_ROOT)."""
    root = os.environ.get("INSTA_BUILD_ROOT")
    if not root:
        return
    import analyze
    root = Path(root).resolve()
    data, cache = root / "data", root / "data" / "cache"
    analyze.ROOT, analyze.DATA, analyze.CACHE, analyze.OVERRIDES = root, data, cache, data / "overrides.json"
    mod = sys.modules.get("privacy")
    if mod is not None:
        mod.MODELS = cache / "models"
        mod.FACE_MODEL = mod.MODELS / Path(mod.FACE_MODEL).name
        mod.TRACK_MODEL = mod.MODELS / Path(mod.TRACK_MODEL).name
        mod.DATA, mod.THUMBS = data / "privacy", cache / "privacy"


def _privacy():
    import privacy
    _relocate()
    return privacy


RENDER_BIN = Path(os.environ.get("INSTA_RENDER_BIN", "insta-render"))


def _gpu_engine_available():
    return RENDER_BIN.exists() and os.environ.get("INSTA_NVENC") == "1"


def _session(s):
    import insta360
    return insta360.Session(s["id"], s["date"], s["time"],
                            [insta360.Segment(x["index"], x.get("insv"), x.get("lrv"), x.get("offset", 0.0),
                                              x.get("duration", 0.0)) for x in s["segments"]], s.get("parts", []))


def _mats(mats):
    return [np.array(m, float).reshape(3, 3) for m in mats]


# ------------------------------------------------------------------ copies de server.py

def clip_parts(session, result, clip):
    """Découpe un clip (temps de session) en morceaux par segment de fichier."""
    parts = []
    for seg, info in zip(session.segments, result["segments"]):
        s = max(clip["start"], info["offset"])
        e = min(clip["end"], info["offset"] + info["duration"])
        if e - s > 0.05:
            parts.append((seg, s - info["offset"], e - s))
    return parts


def source_fps(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate",
                          "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip()
    return Fraction(out)


def run_part_process(job, cmd, on_progress):
    """Lance un processus et suit sa sortie ; son identifiant est transmis (annulation)."""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    job["proc"] = proc
    for line in proc.stdout:
        on_progress(line)
    if proc.wait() != 0:
        raise RuntimeError(proc.stderr.read()[-2000:])


def level_matrix_at(clip, result, horizon_data, t_session):
    import geometry
    import horizon
    mode = geometry.clip_horizon_mode(clip)
    if mode == "aucun":
        return None
    if mode == "auto" and horizon_data and horizon_data.get("up"):
        return horizon.level_at(horizon_data, t_session)
    return geometry.tilt_matrix(result.get("tilt"))


def part_targets(clip, result, horizon_data, seg_offset, ss, dur, fd):
    import geometry
    out = []
    for i in range(int(math.ceil(dur / fd)) + 1):
        t = i * fd
        t_session = seg_offset + ss + t
        level = level_matrix_at(clip, result, horizon_data, t_session)
        v = geometry.clip_view_at(clip, t_session - clip["start"])
        out.append((t, geometry.view_matrix(v["yaw"], v["pitch"], level, v["roll"]), v["fov"]))
    return out


def analyze_privacy(job, items, sessions, horizons, force=False, label=""):
    """Analyse de confidentialité : visages et plaques de chaque clip, dans son cadrage."""
    import analyze
    import geometry
    import horizon
    privacy = _privacy()
    if not _gpu_engine_available():
        raise ValueError("l'analyse demande le moteur GPU (render/ + NVENC)")
    job["message"] = f"{label}chargement des modèles"
    detector = privacy.Detector()
    work = analyze.CACHE / "privacy_render"
    work.mkdir(parents=True, exist_ok=True)
    total = sum(c["end"] - c["start"] for _, c in items) or 1
    done, found, skipped = 0.0, 0, 0
    for n, (sid, clip) in enumerate(items):
        dur_clip = clip["end"] - clip["start"]
        key = privacy.view_key(clip)
        if not force and privacy.load(sid).get(clip["id"], {}).get("key") == key:
            done += dur_clip
            skipped += 1
            continue
        session, result = sessions[sid]
        horizon_data = horizons.get(sid)
        if geometry.clip_horizon_mode(clip) == "auto" and horizon_data is None:
            horizon_data = horizon.compute_range(session, result, clip["start"] - 3, clip["end"] + 3)
        tracks = []
        for k, (seg, ss, dur) in enumerate(clip_parts(session, result, clip)):
            if not seg.insv:
                raise ValueError(f"fichier .insv manquant pour {sid}")
            fps = source_fps(seg.insv)
            seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
            targets = part_targets(clip, result, horizon_data, seg_offset, ss, dur, 1 / float(fps))
            h264, spec = work / f"{sid}_{clip['id']}_{k}.h264", work / f"{sid}_{clip['id']}_{k}.json"
            spec.write_text(json.dumps({
                "source": seg.insv, "start": ss, "duration": dur, "width": privacy.AW, "height": privacy.AH,
                "fov": targets[0][2], "fovs": [float(f) for _, _, f in targets], "cq": 21, "masks": [],
                "matrices": [[float(v) for v in m.flatten()] for _, m, _ in targets], "output": str(h264)}))
            job["message"] = f"{label}clip {n + 1}/{len(items)} : rendu"
            run_part_process(job, [str(RENDER_BIN), str(spec)], lambda _: None)
            job["message"] = f"{label}clip {n + 1}/{len(items)} : détection"

            def progress(f, base=done, d=dur):
                job["progress"] = min(1.0, (base + f * d) / total)
            part = privacy.analyze_clip(h264, [seg_offset + ss + t for t, _, _ in targets],
                                        [m for _, m, _ in targets], [f for _, _, f in targets],
                                        sid, f"{clip['id']}_{k}", detector, progress)
            for t in part:
                t["id"] = len(tracks)
                tracks.append(t)
            h264.unlink(missing_ok=True)
            spec.unlink(missing_ok=True)
            done += dur
        data = privacy.load(sid)
        data[clip["id"]] = {**data.get(clip["id"], {}), "key": key, "tracks": tracks}
        privacy.save(sid, data)
        found += len(tracks)
    return found, skipped


def render_local(job, session, result, a, b, M, fov, size):
    """Rendu (moteur GPU) d'une vue carrée fixe M entre a et b, une image sur MANUAL_STEP."""
    import analyze
    privacy = _privacy()
    work = analyze.CACHE / "privacy_render"
    work.mkdir(parents=True, exist_ok=True)
    frames, times, fps = [], [], 30000 / 1001
    for k, (seg, ss, dur) in enumerate(clip_parts(session, result, {"start": a, "end": b})):
        if not seg.insv:
            raise ValueError("fichier .insv manquant")
        fps = float(source_fps(seg.insv))
        seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
        h264, spec = work / f"local_{os.getpid()}_{k}.h264", work / f"local_{os.getpid()}_{k}.json"
        spec.write_text(json.dumps({"source": seg.insv, "start": ss, "duration": dur, "width": size, "height": size,
                                    "fov": fov, "fovs": [fov], "cq": 23, "masks": [],
                                    "matrices": [[float(v) for v in M.flatten()]], "output": str(h264)}))
        run_part_process(job, [str(RENDER_BIN), str(spec)], lambda _: None)
        part = privacy.decode(h264, size, privacy.MANUAL_STEP)
        times += [seg_offset + ss + i * privacy.MANUAL_STEP / fps for i in range(len(part))]
        frames += part
        h264.unlink(missing_ok=True)
        spec.unlink(missing_ok=True)
    return frames, times, fps


FOLLOW_CHUNK_S = 3.0      # suivi d'un compagnon : vue locale recentrée toutes les 3 s


def track_sphere(job, session, result, t0, d0, ax, ay, a, b):
    """Suit un objet sur la sphère entre a et b depuis t0 : {t: direction}."""
    privacy = _privacy()
    size, fov = privacy.MANUAL_SIZE, privacy.local_fov(ax, ay)
    track = {t0: np.asarray(d0, float)}
    total = max(b - a, 1e-6)
    for direction in (+1, -1):
        d, sx, sy, t = np.asarray(d0, float), ax, ay, t0
        while (t < b - 0.1) if direction > 0 else (t > a + 0.1):
            c0, c1 = (t, min(b, t + FOLLOW_CHUNK_S)) if direction > 0 else (max(a, t - FOLLOW_CHUNK_S), t)
            M = privacy.local_view(d)
            frames, times, fps = render_local(job, session, result, c0, c1, M, fov, size)
            if len(frames) < 2:
                break
            k0 = int(np.argmin(np.abs(np.array(times) - t)))
            boxes = privacy.follow(frames, k0, privacy.tight_box(d, sx, sy, M, fov, size), direction, fps)
            for k, box in boxes.items():
                dk, bx, by = privacy.box_to_sphere(box, M, fov, size, size)
                track[times[k]] = dk
            kend = max(boxes) if direction > 0 else min(boxes)
            reached_end = kend == (len(frames) - 1 if direction > 0 else 0)
            d, sx, sy = privacy.box_to_sphere(boxes[kend], M, fov, size, size)
            t = times[kend]
            job["progress"] = min(1.0, len(track) * privacy.MANUAL_STEP / fps / total)
            job["message"] = f"suivi : {min(track):.0f}–{max(track):.0f} s"
            if not reached_end:   # objet perdu : on s'arrête dans ce sens
                break
    return dict(sorted(track.items()))


def manual_zone(job, session, result, sid, clip, t0, d0, ax, ay, track_it):
    """Zone tracée à la main à l'instant t0 : suivie dans le temps (VitTrack) ou fixe sur le clip."""
    privacy = _privacy()
    d0 = np.asarray(d0, float) / np.linalg.norm(d0)
    thumb = None
    if not track_it:   # fixe dans le repère caméra (ex. élément solidaire de la moto)
        ts = np.arange(clip["start"], clip["end"] + 0.5, 0.5)
        samples = [[round(float(t), 3), *(round(float(x), 5) for x in d0), round(ax, 5), round(ay, 5)] for t in ts]
    else:
        if not _gpu_engine_available():
            raise ValueError("le suivi demande le moteur GPU (render/ + NVENC)")
        size, fov = privacy.MANUAL_SIZE, privacy.local_fov(ax, ay)
        M = privacy.local_view(d0)
        a = max(clip["start"], t0 - privacy.MANUAL_WINDOW_S)
        b = min(clip["end"], t0 + privacy.MANUAL_WINDOW_S)
        job["message"] = "rendu autour de la zone"
        frames, times, fps = render_local(job, session, result, a, b, M, fov, size)
        if not frames:
            raise ValueError("aucune image rendue autour de la zone")
        job["message"] = "suivi de la zone"
        k0 = int(np.argmin(np.abs(np.array(times) - t0)))
        box0 = privacy.tight_box(d0, ax, ay, M, fov, size)
        boxes = privacy.follow(frames, k0, box0, -1, fps) | privacy.follow(frames, k0, box0, +1, fps)
        samples = []
        for k in sorted(boxes):
            d, bx, by = privacy.box_to_sphere(boxes[k], M, fov, size, size)
            samples.append([round(times[k], 3), *(round(float(x), 5) for x in d), round(bx, 5), round(by, 5)])
        import cv2
        x, y, w, h = box0
        m = max(w, h)
        crop = frames[k0][max(0, y - m):y + h + m, max(0, x - m):x + w + m]
        if crop.size:
            thumb = f"{sid}_{clip['id']}_manuel_{secrets.token_hex(3)}.jpg"
            privacy.THUMBS.mkdir(parents=True, exist_ok=True)
            cv2.imwrite(str(privacy.THUMBS / thumb), cv2.resize(crop, (120, max(1, int(120 * crop.shape[0] / crop.shape[1])))))
    data = privacy.load(sid)
    manual = data.setdefault(clip["id"], {}).setdefault("manual", [])
    manual.append({"id": f"m{secrets.token_hex(3)}", "kind": "manuel" if track_it else "fixe", "conf": 1.0,
                   "enabled": True, "thumb": thumb, "samples": samples})
    privacy.save(sid, data)
    span = samples[-1][0] - samples[0][0] if samples else 0
    return f"zone {'suivie' if track_it else 'fixe'} sur {span:.1f} s"


# ------------------------------------------------------------------ fonctions appelées par pending.rs

def privacy_frame_boxes(a):
    return _privacy().frame_boxes(a["times"], _mats(a["mats"]), a["fovs"], a["tracks"], a["W"], a["H"])


def privacy_blur_video(a):
    privacy = _privacy()
    detector = privacy.Detector() if a.get("detect") else None
    job = Job()
    return privacy.blur_video(Path(a["src"]), Path(a["dst"]), a["times"], _mats(a["mats"]), a["fovs"], a["tracks"],
                              a["W"], a["H"], a["encoder_args"], detector,
                              lambda f: job.__setitem__("blur_progress", float(f)))


def _sessions(a):
    s = a.get("sessions", {})
    return ({sid: (_session(x["session"]), x["result"]) for sid, x in s.items()},
            {sid: x["horizon"] for sid, x in s.items() if x.get("horizon")})


def privacy_analyze(a):
    sessions, horizons = _sessions(a)
    return list(analyze_privacy(Job(), [tuple(x) for x in a["items"]], sessions, horizons, a.get("force", False),
                                a.get("label", "")))


def privacy_manual_zone(a):
    sessions, _ = _sessions(a)
    session, result = sessions[a["sid"]]
    return manual_zone(Job(), session, result, a["sid"], a["clip"], a["t0"], a["d0"], a["ax"], a["ay"], a["track_it"])


def follow_track(a):
    track = track_sphere(Job(), _session(a["session"]), a["result"], a["t0"], a["d0"], a["ax"], a["ay"], a["a"], a["b"])
    return [[float(t), [float(x) for x in d]] for t, d in track.items()]


if __name__ == "__main__":
    fn = globals()[sys.argv[1]]
    args = json.loads(sys.stdin.read() or "null")
    _emit("result", json.dumps(fn(args), default=_default))
