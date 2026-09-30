#!/usr/bin/env python3
"""Serveur local de l'outil de tri : UI, streaming des proxys .lrv, sélections, export ffmpeg.

Usage : python3 server.py [DCIM] [--port 8360] [--host 127.0.0.1]
"""
import argparse
import hashlib
import json
import math
import secrets
import mimetypes
import re
import subprocess
import threading
import traceback
from fractions import Fraction
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import numpy as np

import analyze
import automontage
import endcard
import finishing
import geometry
import horizon
import hyperlapse
import insta360
import musiclib
import privacy
import telemetry

ROOT = Path(__file__).resolve().parent
UI = ROOT / "ui"
SELECTIONS = analyze.DATA / "selections"
EXPORTS = ROOT / "exports"
SETTINGS = analyze.DATA / "settings.json"
SOURCES = analyze.DATA / "sources.json"  # dossiers de vidéos ajoutés depuis l'interface
PROJECT = analyze.DATA / "project.json"  # sessions du projet, ordre et exclusions du montage
THUMBS = analyze.CACHE / "thumbs"
MUSIC = analyze.DATA / "music"  # musiques de fond envoyées depuis l'interface
MUSIC_EXT = {".mp3", ".m4a", ".aac", ".wav", ".ogg", ".opus", ".flac"}
MUSIC_MAX_BYTES = 60 * 1024 * 1024
PROJECT_MIN_S = 60  # sans projet enregistré : sessions d'au moins une minute
RENDER_BIN = ROOT / "render" / "target" / "release" / "insta-render"

# Objectifs X5 : ~195° utiles par fisheye. Dans ffmpeg v360 (dfisheye), yaw 0 = moitié
# droite du .lrv = objectif avant ; pitch négatif = vers le bas.
LENS_FOV = 195
QUALITY = {
    "preview": {"source": "lrv", "height": 720, "crf": 23, "preset": "veryfast"},
    "final": {"source": "insv", "height": 1080, "crf": 20, "preset": "medium"},
}
HEIGHTS = (720, 1080, 1440, 2160)
# Destinations de l'export final. Hors 16:9, taille fixe ; le champ du clip (horizontal en 16:9)
# devient la hauteur de champ du 16:9 en largeur : le carré est un recadrage central, le
# vertical garde la largeur du carré et gagne du champ en haut et en bas.
FORMATS = {
    "standard": {"label": "YouTube / standard 16:9"},
    "vertical": {"label": "Reels / TikTok / Shorts 9:16", "size": (1080, 1920), "max_bitrate": 16_000_000},
    "carre": {"label": "Instagram carré 1:1", "size": (1080, 1080), "max_bitrate": 12_000_000},
    "leger": {"label": "Message (léger, 720p)", "size": (1280, 720), "max_bitrate": 1_400_000, "audio": "96k"},
}
HYPERLAPSE_MBPS_1080 = 12  # plafond du résumé : en accéléré chaque image change, le débit exploserait (~30 Mb/s)

state = {"dcim": None, "sessions": {}, "jobs": {}, "nvenc": None, "horizon": {}, "scan": {"state": "idle"}}
horizon_queue = []
HORIZON_WORKERS = 3  # calculs d'horizon simultanés : une demande urgente n'attend pas la fin d'une longue session
horizon_cv = threading.Condition()


def request_horizon(sid, urgent=False):
    """Met une session dans la file de calcul de l'horizon (en tête si urgent)."""
    with horizon_cv:
        h = state["horizon"].setdefault(sid, {"status": "queued", "progress": 0.0})
        if h["status"] in ("done", "running"):
            return h
        if sid in horizon_queue:
            horizon_queue.remove(sid)
        horizon_queue.insert(0, sid) if urgent else horizon_queue.append(sid)
        h["status"] = "queued"
        horizon_cv.notify()
        return h


def horizon_worker():
    while True:
        with horizon_cv:
            while not horizon_queue:
                horizon_cv.wait()
            sid = horizon_queue.pop(0)
            h = state["horizon"][sid]
            h["status"] = "running"
        try:
            session, result = state["sessions"][sid]
            data = horizon.compute(session, result, analyze.CACHE, lambda f: h.update(progress=f))
            h.update(status="done", progress=1.0, data=data)
        except Exception as e:
            traceback.print_exc()
            h.update(status="error", message=str(e))
        with horizon_cv:
            horizon_cv.notify_all()


def nvenc_available():
    """Teste une fois si l'encodeur GPU NVIDIA fonctionne (pilote assez récent)."""
    if state["nvenc"] is None:
        r = subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=s=256x144:d=0.2",
                            "-c:v", "h264_nvenc", "-f", "null", "-"], capture_output=True, text=True)
        state["nvenc"] = r.returncode == 0
        print("Encodeur GPU NVENC :", "disponible" if state["nvenc"] else "indisponible (x264 utilisé)")
    return state["nvenc"]


def encoder_args(q):
    cap = q.get("max_bitrate")
    rate = ["-maxrate", str(cap), "-bufsize", str(cap)] if cap else []
    if nvenc_available():
        # -cq ≈ -crf de x264 ; p5 = bon compromis qualité/vitesse
        return ["-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr",
                "-cq", str(q["crf"]), "-b:v", "0", *rate, "-pix_fmt", "yuv420p"]
    return ["-c:v", "libx264", "-preset", q["preset"], "-crf", str(q["crf"]), *rate]


def output_size(q):
    """(largeur, hauteur) de sortie selon la destination (16:9 à la hauteur choisie par défaut)."""
    size = FORMATS.get(q.get("format", "standard"), {}).get("size")
    return size or (q["height"] * 16 // 9, q["height"])


def output_fov(fov, w, h):
    """Champ horizontal de sortie pour un champ de clip défini en 16:9 (voir FORMATS)."""
    return fov if w * 9 >= h * 16 - 1 else v_fov_of(fov, 16, 9)
lock = threading.Lock()


def source_folders():
    """Dossier de la ligne de commande (carte SD) puis dossiers ajoutés depuis l'interface."""
    extra = json.loads(SOURCES.read_text()) if SOURCES.exists() else []
    return list(dict.fromkeys([state["dcim"], *extra]))


_durations = {}


def session_duration(session):
    """Durée (s) d'une session d'origine : somme des .lrv (ffprobe, mémorisée)."""
    total = 0.0
    for seg in session.segments:
        key = (seg.lrv, Path(seg.lrv).stat().st_size)
        if key not in _durations:
            _durations[key] = float(analyze._ffprobe_format(seg.lrv, "=duration") or 0)
        total += _durations[key]
    return total


def migrate_block_selections(block, members):
    """Reporte sur le bloc les clips posés sur ses morceaux avant la fusion (décalés dans le temps).

    `members` : [(session d'origine, début dans le bloc en s)]. Le fichier du morceau est
    renommé en .fusionné.json pour ne pas être repris deux fois.
    """
    moved = []
    for member, offset in members:
        path = selections_path(member.id)
        if member.id == block.id or not path.exists():
            continue
        for c in json.loads(path.read_text()):
            moved.append({**c, "start": round(c["start"] + offset, 2), "end": round(c["end"] + offset, 2)})
        path.rename(path.with_suffix(".fusionné.json"))
    if moved:
        clips = sorted(get_selections(block.id) + moved, key=lambda c: c["start"])
        SELECTIONS.mkdir(parents=True, exist_ok=True)
        selections_path(block.id).write_text(json.dumps(clips, indent=1))
        print(f"  {len(moved)} clip(s) reporté(s) sur le bloc {block.id}")


def load_sessions():
    """Analyse les sessions de tous les dossiers présents (analyses en cache réutilisées).

    Une session présente dans deux dossiers (carte + copie) n'est prise qu'une fois : la
    première trouvée, dans l'ordre de source_folders(). Les fichiers qui se suivent sans
    interruption (enregistrement en boucle) forment un seul bloc continu.
    """
    by_id, origin = {}, {}
    for folder in source_folders():
        if not Path(folder).is_dir():
            continue
        for s in insta360.scan(folder):
            if s.id not in by_id:
                by_id[s.id], origin[s.id] = s, folder
    originals = {sid: (s, session_duration(s)) for sid, s in by_id.items()}
    blocks = insta360.merge_continuous([s for s, _ in originals.values()], lambda s: originals[s.id][1])
    for b in blocks:
        if b.parts:
            offsets = [0.0]
            for sid in b.parts[:-1]:
                offsets.append(offsets[-1] + originals[sid][1])
            migrate_block_selections(b, [(originals[sid][0], off) for sid, off in zip(b.parts, offsets)])
    by_id = {b.id: b for b in blocks}
    results = analyze.analyze_sessions(blocks)
    state["sessions"] = {sid: (by_id[sid], {**r, "folder": origin[sid], "parts": len(by_id[sid].parts) or 1})
                         for sid, r in results.items()}


def rescan():
    state["scan"] = {"state": "running", "message": "analyse des dossiers…"}
    try:
        before = set(state["sessions"])
        with lock:
            load_sessions()
        new = sorted(set(state["sessions"]) - before)
        for sid in new:
            request_horizon(sid)
        state["scan"] = {"state": "done", "message": f"{len(new)} nouvelle(s) session(s)", "new": new}
    except Exception as e:
        traceback.print_exc()
        state["scan"] = {"state": "error", "message": str(e)}


def selections_path(sid):
    return SELECTIONS / f"{sid}.json"


def get_settings():
    cfg = json.loads(SETTINGS.read_text()) if SETTINGS.exists() else {}
    return {"masks": cfg.get("masks", []), "telemetry": {**telemetry.DEFAULTS, **cfg.get("telemetry", {})},
            "privacy": {"enabled": False, **cfg.get("privacy", {})}}


def get_selections(sid):
    """Clips d'une session ; ceux d'avant les identifiants en reçoivent un (ordre du montage)."""
    p = selections_path(sid)
    clips = json.loads(p.read_text()) if p.exists() else []
    if any("id" not in c for c in clips):
        for c in clips:
            c.setdefault("id", secrets.token_hex(4))
        p.write_text(json.dumps(clips, indent=1))
    return clips


def get_project():
    """Projet : sessions retenues, ordre libre du montage [[sid, id du clip]], clips exclus."""
    if PROJECT.exists():
        proj = json.loads(PROJECT.read_text())
    else:
        proj = {"sessions": [sid for sid, (_, r) in sorted(state["sessions"].items())
                             if r["duration"] >= PROJECT_MIN_S or get_selections(sid)]}
    return {"sessions": [x for x in proj.get("sessions", []) if x in state["sessions"]],
            "order": proj.get("order", []), "excluded": proj.get("excluded", []),
            "style": finishing.clean(proj.get("style"))}


def music_files():
    return sorted(p.name for p in MUSIC.glob("*") if p.suffix.lower() in MUSIC_EXT) if MUSIC.is_dir() else []


def montage_items(proj=None, with_excluded=False):
    """Clips du montage dans l'ordre : ordre enregistré, puis les nouveaux clips chronologiquement."""
    proj = proj or get_project()
    clips = {(sid, c["id"]): c for sid in proj["sessions"] for c in get_selections(sid)}
    order = [tuple(x) for x in proj["order"] if tuple(x) in clips]
    rest = sorted(set(clips) - set(order),
                  key=lambda k: state["sessions"][k[0]][1]["utc_t0"] + clips[k]["start"])
    excluded = {tuple(x) for x in proj["excluded"]}
    if with_excluded:
        return [(sid, clips[(sid, cid)], (sid, cid) in excluded) for sid, cid in order + rest]
    return [(sid, clips[(sid, cid)]) for sid, cid in order + rest if (sid, cid) not in excluded]


def minimap(sid, clip_id, size=320):
    """Aperçu PNG de la mini-carte d'export : tracés des sessions du montage, clip en couleur."""
    from PIL import Image
    sids = list(dict.fromkeys([sid, *(x for x, _ in montage_items())]))
    clip = next((c for c in get_selections(sid) if c["id"] == clip_id), {"start": 0, "end": -1})
    tag = "|".join([*sids, sid, str(clip_id), str(clip["start"]), str(clip["end"]), str(size),
                    *(str(state["sessions"][x][1]["utc_t0"] + state["sessions"][x][1]["offset_s"]) for x in sids)])
    out = THUMBS / f"map_{hashlib.sha1(tag.encode()).hexdigest()[:16]}.png"
    if out.exists():
        return out
    result = state["sessions"][sid][1]
    panel, project, _ = telemetry._map_panel(result, clip, [state["sessions"][x][1] for x in sids], size, size / 0.26)
    if clip["end"] > clip["start"]:
        lat, lon = telemetry._series(result, "lat"), telemetry._series(result, "lon")
        if lat is not None:
            D = max(8, int(size * 0.06)) // 2 * 2
            x, y = project(lat[int(clip["start"])], lon[int(clip["start"])])
            dot = telemetry._dot(D)
            x0, y0 = int(round(x - D / 2)), int(round(y - D / 2))
            if 0 <= x0 <= size - D and 0 <= y0 <= size - D:
                telemetry._over(panel[y0:y0 + D, x0:x0 + D], dot)
    THUMBS.mkdir(parents=True, exist_ok=True)
    Image.fromarray(np.clip(panel, 0, 255).astype(np.uint8), "RGBA").save(out)
    return out


def thumbnail(sid, t, yaw=0.0, pitch=-10.0, fov=100.0, width=320):
    """Vignette JPEG (vue plane) d'une session à l'instant t, mise en cache."""
    session, result = state["sessions"][sid]
    name = hashlib.sha1(f"{sid}|{t:.1f}|{yaw:.1f}|{pitch:.1f}|{fov:.0f}|{width}".encode()).hexdigest()[:16]
    out = THUMBS / f"{name}.jpg"
    if out.exists():
        return out
    for seg, info in zip(session.segments, result["segments"]):
        if info["offset"] <= t < info["offset"] + info["duration"] or info is result["segments"][-1]:
            local = max(0.0, min(t - info["offset"], info["duration"] - 0.5))
            break
    height = width * 9 // 16
    vfov = v_fov_of(fov, width, height)
    THUMBS.mkdir(parents=True, exist_ok=True)
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-ss", f"{local:.2f}", "-i", seg.lrv, "-frames:v", "1",
                    "-vf", f"v360=input=dfisheye:ih_fov={LENS_FOV}:iv_fov={LENS_FOV}:output=flat:yaw={yaw}:pitch={pitch}"
                           f":h_fov={fov}:v_fov={vfov:.2f}:w={width}:h={height}",
                    "-q:v", "5", str(out)], check=True, timeout=60)
    return out


# ---------------------------------------------------------------- export ffmpeg

def source_size(path, source):
    """Taille de l'image double fisheye (les deux flux .insv sont juxtaposés)."""
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                          "stream=width,height", "-of", "csv=p=0", path], capture_output=True, text=True).stdout
    w, h = map(int, out.strip().split(",")[:2])
    return (w * 2, h) if source == "insv" else (w, h)


def mask_filters(masks, size, label):
    """Floute les zones masquées (coordonnées 0..1 de l'image double fisheye)."""
    W, H = size
    chain = []
    for k, m in enumerate(masks):
        x, y = int(m["x"] * W) // 2 * 2, int(m["y"] * H) // 2 * 2
        w, h = max(8, int(m["w"] * W) // 2 * 2), max(8, int(m["h"] * H) // 2 * 2)
        w, h = min(w, W - x), min(h, H - y)
        r = max(2, min(w, h) // 5)
        chain.append(f"[{label}{k}]split[b{k}][c{k}];[c{k}]crop={w}:{h}:{x}:{y},boxblur={r}:3[m{k}];"
                     f"[b{k}][m{k}]overlay={x}:{y}[{label}{k + 1}]")
    return chain


def level_matrix_at(clip, result, horizon_data, t_session):
    """Redressement d'un clip à l'instant t selon son mode (auto → image, sinon support)."""
    mode = geometry.clip_horizon_mode(clip)
    if mode == "aucun":
        return None
    if mode == "auto" and horizon_data and horizon_data.get("up"):
        return horizon.level_at(horizon_data, t_session)
    return geometry.tilt_matrix(result.get("tilt"))


def level_commands(targets, path, w, h):
    """Écrit un fichier sendcmd suivant des cadrages cibles (un par image).

    Les commandes de rotation de v360 sont cumulatives (R ← R·D, vérifié) : on envoie
    donc à chaque pas la rotation différentielle D = Rᵀ_précédent·R_cible. Le champ
    (h_fov/v_fov) est envoyé en valeur absolue quand il change.
    Retourne les angles initiaux, à passer en options du filtre.
    """
    lines = []
    for k, (t, target, fov) in enumerate(targets):
        if not k:
            continue
        cmds = []
        dy, dp, dr = geometry.v360_angles(targets[k - 1][1].T @ target)
        if max(abs(dy), abs(dp), abs(dr)) > 0.01:
            cmds.append(f"v360@lv yaw {dy:.4f}, v360@lv pitch {dp:.4f}, v360@lv roll {dr:.4f}")
        if abs(fov - targets[k - 1][2]) > 0.01:
            cmds.append(f"v360@lv h_fov {fov:.3f}, v360@lv v_fov {v_fov_of(fov, w, h):.3f}")
        if cmds:
            lines.append(f"{t:.3f} " + ", ".join(cmds) + ";")
    path.write_text("\n".join(lines) + "\n")
    return geometry.v360_angles(targets[0][1])


def part_targets(clip, result, horizon_data, seg_offset, ss, dur, fd):
    """(t relatif au morceau, rotation écran → caméra, champ horizontal °) pour chaque image.

    Le cadrage suit les points clés du clip (geometry.clip_view_at), l'horizon son mode.
    """
    out = []
    for i in range(int(math.ceil(dur / fd)) + 1):
        t = i * fd
        t_session = seg_offset + ss + t
        level = level_matrix_at(clip, result, horizon_data, t_session)
        v = geometry.clip_view_at(clip, t_session - clip["start"])
        out.append((t, geometry.view_matrix(v["yaw"], v["pitch"], level, v["roll"]), v["fov"]))
    return out


def v_fov_of(h_fov, w, h):
    return math.degrees(2 * math.atan(math.tan(math.radians(h_fov) / 2) * h / w))


def v360_filter(clip, w, h, source, masks=(), size=None, angles=None, cmdfile=None):
    h_fov = float(clip.get("fov", 100))
    v_fov = math.degrees(2 * math.atan(math.tan(math.radians(h_fov) / 2) * h / w))
    yaw, pitch, roll = angles or (clip.get("yaw", 0), clip.get("pitch", 0), 0)
    v360 = (f"v360@lv=input=dfisheye:ih_fov={LENS_FOV}:iv_fov={LENS_FOV}:output=flat"
            f":yaw={yaw:.3f}:pitch={pitch:.3f}:roll={roll:.3f}"
            f":h_fov={h_fov:.2f}:v_fov={v_fov:.2f}:w={w}:h={h},setsar=1,format=yuv420p")
    if cmdfile:
        v360 = f"sendcmd=f='{cmdfile}',{v360}"
    # .insv : flux 0 = objectif avant, flux 1 = arrière → on reproduit la disposition du .lrv.
    inp = "[0:v:1][0:v:0]hstack" if source == "insv" else "[0:v:0]null"
    return ";".join([f"{inp}[s0]", *mask_filters(masks, size, "s"), f"[s{len(masks)}]{v360}[v]"])


def clip_parts(session, result, clip):
    """Découpe un clip (temps de session) en morceaux par segment de fichier."""
    parts = []
    for seg, info in zip(session.segments, result["segments"]):
        s = max(clip["start"], info["offset"])
        e = min(clip["end"], info["offset"] + info["duration"])
        if e - s > 0.05:
            parts.append((seg, s - info["offset"], e - s))
    return parts


def gpu_engine_available():
    """Moteur Rust/CUDA compilé (render/) et NVENC utilisable."""
    return RENDER_BIN.exists() and nvenc_available()


def source_fps(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate",
                          "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip()
    return Fraction(out)


def run_part_process(job, cmd, on_progress):
    """Lance un processus d'export et suit sa progression (secondes rendues)."""
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    job["proc"] = proc
    for line in proc.stdout:
        on_progress(line)
    if proc.wait() != 0:
        if job.get("cancelled"):
            raise ValueError("annulé")
        raise RuntimeError(proc.stderr.read()[-2000:])


def export_part_ffmpeg(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, size, out_w, out_h, out, report):
    seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
    targets = [(t, m, output_fov(f, out_w, out_h))
               for t, m, f in part_targets(clip, result, horizon_data, seg_offset, ss, dur, 1 / float(source_fps(src)))]
    cmdfile = out.with_suffix(".cmd")
    angles = level_commands(targets, cmdfile, out_w, out_h)
    # cadrage immobile (horizon fixe, sans point clé) : aucune commande, et sendcmd refuse un fichier vide
    has_cmds = bool(cmdfile.read_text().strip())
    graph = v360_filter({**clip, "fov": targets[0][2]}, out_w, out_h, q["source"], masks, size, angles,
                        cmdfile if has_cmds else None)
    cmd = ["ffmpeg", "-v", "error", "-y", "-ss", f"{ss:.3f}", "-t", f"{dur:.3f}", "-i", src,
           "-filter_complex", graph, "-map", "[v]", "-map", "0:a:0?", *encoder_args(q),
           "-c:a", "aac", "-b:a", q.get("audio", "160k"), "-ar", "48000", "-ac", "2",
           "-movflags", "+faststart", "-progress", "pipe:1", "-nostats", str(out)]

    def progress(line):
        if line.startswith("out_time_us=") and line[12:].strip().isdigit():
            report(int(line[12:]) / 1e6)
    run_part_process(job, cmd, progress)
    return [seg_offset + ss + t for t, _, _ in targets], [m for _, m, _ in targets], [f for _, _, f in targets]


def export_part_gpu(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, out_w, out_h, out, report):
    """Rendu NVDEC → CUDA → NVENC (render/), puis multiplexage du son par ffmpeg."""
    fps = source_fps(src)
    fd = 1 / float(fps)
    seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
    targets = part_targets(clip, result, horizon_data, seg_offset, ss, dur, fd)
    matrices = [[float(v) for v in m.flatten()] for _, m, _ in targets]
    fovs = [float(output_fov(f, out_w, out_h)) for _, _, f in targets]
    h264 = out.with_suffix(".h264")
    spec = out.with_suffix(".json")
    spec.write_text(json.dumps({
        "source": src, "start": ss, "duration": dur, "width": out_w, "height": out_h,
        "fov": fovs[0], "fovs": fovs, "cq": int(q["crf"]), "max_bitrate": int(q.get("max_bitrate", 0)),
        "masks": [[m["x"], m["y"], m["w"], m["h"]] for m in masks],
        "matrices": matrices, "output": str(h264),
    }))

    def progress(line):
        if line.startswith("frame="):
            report(int(line[6:]) * fd)
    run_part_process(job, [str(RENDER_BIN), str(spec)], progress)
    run_part_process(job, ["ffmpeg", "-v", "error", "-y", "-framerate", str(fps), "-i", str(h264),
                           "-ss", f"{ss:.3f}", "-t", f"{dur:.3f}", "-i", src, "-map", "0:v", "-map", "1:a:0?",
                           "-c:v", "copy", "-c:a", "aac", "-b:a", q.get("audio", "160k"), "-ar", "48000", "-ac", "2",
                           "-movflags", "+faststart", str(out)], lambda _: None)
    h264.unlink(missing_ok=True)
    return [seg_offset + ss + t for t, _, _ in targets], [m for _, m, _ in targets], fovs


def run_export(key, clips, quality, opts):
    """Exporte une liste de clips [(session, clip)] dans l'ordre donné (une session ou un montage)."""
    job = state["jobs"][key]
    try:
        q = {**QUALITY[quality], **opts, **FORMATS.get(opts.get("format", "standard"), {})}
        out_w, out_h = output_size(q)
        name = f"{quality}_{out_h}p" if q.get("format", "standard") == "standard" else q["format"]
        sids = list(dict.fromkeys(sid for sid, _ in clips))
        settings = get_settings()
        masks, tel_opts = settings["masks"], settings["telemetry"]
        blur_on = settings["privacy"]["enabled"]
        sizes = {}
        if not clips:
            raise ValueError("aucun clip sélectionné")
        total_s = sum(c["end"] - c["start"] for _, c in clips)
        if q.get("format") == "vertical" and total_s > 90:
            job["warning"] = f"{int(total_s // 60)} min {int(total_s % 60):02d} : long pour un Reel (90 s) ou un Short (60 s)"
        gpu = q["source"] == "insv" and gpu_engine_available()
        job["engine"] = "GPU" if gpu else "ffmpeg"
        out_dir = EXPORTS / key / name
        out_dir.mkdir(parents=True, exist_ok=True)
        todo = [(i, sid, c, p) for i, (sid, c) in enumerate(clips)
                for p in clip_parts(state["sessions"][sid][0], state["sessions"][sid][1], c)]
        total = sum(p[2] for *_, p in todo)
        done, files = 0.0, []
        clip_horizon = {}
        for n, (i, sid, clip, (seg, ss, dur)) in enumerate(todo):
            session, result = state["sessions"][sid]
            # Horizon : analyse complète si déjà prête, sinon seulement les portions des clips.
            full = state["horizon"].get(sid, {})
            horizon_data = full.get("data") if full.get("status") == "done" else None
            if geometry.clip_horizon_mode(clip) == "auto" and horizon_data is None:
                if i not in clip_horizon:
                    job["message"] = f"horizon du clip {i + 1}/{len(clips)}"
                    clip_horizon[i] = horizon.compute_range(session, result, clip["start"] - 3, clip["end"] + 3)
                horizon_data = clip_horizon[i]
            src = seg.insv if q["source"] == "insv" else seg.lrv
            if not src:
                raise ValueError(f"fichier {q['source']} manquant pour le segment {seg.index} de {sid}")
            out = out_dir / f"part_{n:03d}.mp4"
            job["message"] = f"clip {i + 1}/{len(clips)} ({job['engine']})"

            def report(t, base=done):
                job["progress"] = min(1.0, (base + t) / total)
            if gpu:
                views = export_part_gpu(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, out_w, out_h, out, report)
            else:
                if src not in sizes:
                    sizes[src] = source_size(src, q["source"])
                views = export_part_ffmpeg(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, sizes[src], out_w, out_h, out, report)
            if blur_on:
                tracks = privacy.all_tracks(privacy.load(sid).get(clip.get("id")))
                if any(t.get("enabled", True) for t in tracks):
                    job["message"] = f"clip {i + 1}/{len(clips)} : floutage"
                    blurred = out.with_name(out.stem + "_flou.mp4")
                    privacy.blur_video(out, blurred, *views, tracks, out_w, out_h, encoder_args(q))
                    blurred.replace(out)
            if tel_opts["enabled"] and result.get("gps_coverage", 0) > 0.3:
                job["message"] = f"clip {i + 1}/{len(clips)} : télémétrie"
                seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
                with_tel = out.with_name(out.stem + "_tel.mp4")
                if telemetry.overlay(out, with_tel, result, clip, seg_offset + ss, dur, out_w, out_h, tel_opts,
                                     first_part=abs(seg_offset + ss - clip["start"]) < 0.5,
                                     encoder_args=encoder_args(q), workdir=out_dir / f"tel_{n:03d}",
                                     tracks=[state["sessions"][x][1] for x in sids]):
                    with_tel.replace(out)
            done += dur
            files.append((i, out))
        job["message"] = "assemblage"
        prefix = f"montage_{min(sids)[4:12]}_{len(clips)}clips" if key == "montage" else key
        final = EXPORTS / f"{prefix}_{name}.mp4"
        style = get_project()["style"] if key == "montage" else None
        if style and not finishing.is_plain(style):
            # un fichier par clip (morceaux d'un même clip recollés sans transition), puis finition
            clip_files = []
            for i in dict.fromkeys(i for i, _ in files):
                parts = [f for j, f in files if j == i]
                if len(parts) == 1:
                    clip_files.append(parts[0])
                    continue
                joined = out_dir / f"clip_{i:03d}.mp4"
                (out_dir / f"clip_{i:03d}.txt").write_text("".join(f"file '{f.name}'\n" for f in parts))
                subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0",
                                "-i", str(out_dir / f"clip_{i:03d}.txt"), "-c", "copy", str(joined)], check=True)
                clip_files.append(joined)
            music = MUSIC / style["music"] if style["music"] in music_files() else None
            credits = musiclib.credit_lines(style["music"]) if music else []
            card = None
            if style["end_card"]:
                card = out_dir / "fin.png"
                endcard.render([state["sessions"][x][1] for x in sids], out_w, out_h, card, style["title"], credits)
            job["message"] = "transitions, titre, musique"
            finishing.finish(clip_files, final, style, encoder_args(q), out_w, out_h, music, q.get("audio", "160k"),
                             end_card=card, credits=credits)
        else:
            listing = out_dir / "concat.txt"
            listing.write_text("".join(f"file '{f.name}'\n" for _, f in files))
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
                            "-c", "copy", "-movflags", "+faststart", str(final)], check=True)
        job.update(state="done", progress=1.0, message=f"terminé ({job['engine']})", output=final.name)
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


def run_privacy(items, force=False):
    """Analyse de confidentialité : visages et plaques de chaque clip, dans son cadrage."""
    job = state["jobs"]["privacy"]
    try:
        if not gpu_engine_available():
            raise ValueError("l'analyse demande le moteur GPU (render/ + NVENC)")
        job["message"] = "chargement des modèles"
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
            session, result = state["sessions"][sid]
            full = state["horizon"].get(sid, {})
            horizon_data = full.get("data") if full.get("status") == "done" else None
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
                job["message"] = f"clip {n + 1}/{len(items)} : rendu"
                run_part_process(job, [str(RENDER_BIN), str(spec)], lambda _: None)
                job["message"] = f"clip {n + 1}/{len(items)} : détection"

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
        msg = f"{found} zone(s) détectée(s)" + (f", {skipped} clip(s) déjà à jour" if skipped else "")
        job.update(state="done", progress=1.0, message=msg)
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


def render_local(job, session, result, a, b, M, fov, size):
    """Rendu (moteur GPU) d'une vue carrée fixe M entre a et b (temps session), une image sur
    MANUAL_STEP : (images BGR, temps session, cadence source)."""
    work = analyze.CACHE / "privacy_render"
    work.mkdir(parents=True, exist_ok=True)
    frames, times, fps = [], [], 30000 / 1001
    for k, (seg, ss, dur) in enumerate(clip_parts(session, result, {"start": a, "end": b})):
        if not seg.insv:
            raise ValueError("fichier .insv manquant")
        fps = float(source_fps(seg.insv))
        seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
        h264, spec = work / f"local_{k}.h264", work / f"local_{k}.json"
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
FOLLOW_KEY_S = 0.5        # un point clé toutes les 0,5 s
FOLLOW_SMOOTH_S = 0.6     # lissage des angles (pas de tremblement du cadrage)


def track_sphere(job, session, result, t0, d0, ax, ay, a, b):
    """Suit un objet sur la sphère entre a et b depuis t0 : {t: direction}.

    Par tranches de FOLLOW_CHUNK_S, la vue locale est recentrée sur la dernière position :
    l'objet peut faire le tour de la moto sans sortir de la vue."""
    import cv2  # noqa: F401  (chargé par privacy.follow)
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


def run_follow(sid, clip_id, t0, d0, ax, ay, view0):
    """Cadrage qui suit un compagnon : points clés du clip générés depuis le suivi."""
    job = state["jobs"]["follow"]
    try:
        if not gpu_engine_available():
            raise ValueError("le suivi demande le moteur GPU (render/ + NVENC)")
        session, result = state["sessions"][sid]
        clip = next(c for c in get_selections(sid) if c["id"] == clip_id)
        track = track_sphere(job, session, result, t0, d0, ax, ay, clip["start"], clip["end"])
        full = state["horizon"].get(sid, {})
        horizon_data = full.get("data") if full.get("status") == "done" else None
        ts = np.array(list(track))
        ang = []
        for t, d in track.items():   # angles de l'objet dans le repère redressé du clip
            L = level_matrix_at(clip, result, horizon_data, t)
            v = d if L is None else L.T @ d
            ang.append((math.degrees(math.atan2(v[0], v[2])), math.degrees(math.asin(max(-1.0, min(1.0, v[1]))))))
        yaw = np.degrees(np.unwrap(np.radians([a for a, _ in ang])))
        pitch = np.array([p for _, p in ang])
        # composition gardée : décalage entre l'objet et le centre de la vue au moment du tracé
        k0 = int(np.argmin(np.abs(ts - t0)))
        dyaw, dpitch = view0["yaw"] - yaw[k0], view0["pitch"] - pitch[k0]
        grid = np.arange(ts[0], ts[-1] + 1e-6, FOLLOW_KEY_S)
        sig = FOLLOW_SMOOTH_S
        keys = []
        for t in grid:
            w = np.exp(-((ts - t) / sig) ** 2 / 2)
            y, p = float((w * yaw).sum() / w.sum()) + dyaw, float((w * pitch).sum() / w.sum()) + dpitch
            keys.append({"t": round(float(t - clip["start"]), 2), "yaw": round((y + 180) % 360 - 180, 2),
                         "pitch": round(max(-89.0, min(89.0, p)), 2), "roll": round(float(view0.get("roll", 0)), 2),
                         "fov": round(float(view0["fov"]), 1), "curve": "linear"})
        with lock:
            clips = get_selections(sid)
            c = next(c for c in clips if c["id"] == clip_id)
            c["keyframes"] = keys
            c.pop("roll_keys", None)
            c.pop("auto", None)
            selections_path(sid).write_text(json.dumps(clips, indent=1))
        span = ts[-1] - ts[0]
        job.update(state="done", progress=1.0, clip=clip_id, sid=sid,
                   message=f"cadrage suivi sur {span:.1f} s ({len(keys)} points clés)"
                           + ("" if span >= clip["end"] - clip["start"] - 0.5 else " — objet perdu avant la fin du clip"))
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


def run_manual_zone(sid, clip, t0, d0, ax, ay, track_it):
    """Zone tracée à la main à l'instant t0 : suivie dans le temps (VitTrack) ou fixe sur le clip."""
    job = state["jobs"]["privacy"]
    try:
        session, result = state["sessions"][sid]
        d0 = np.asarray(d0, float) / np.linalg.norm(d0)
        thumb, n = None, 0
        if not track_it:   # fixe dans le repère caméra (ex. élément solidaire de la moto)
            ts = np.arange(clip["start"], clip["end"] + 0.5, 0.5)
            samples = [[round(float(t), 3), *(round(float(x), 5) for x in d0), round(ax, 5), round(ay, 5)] for t in ts]
        else:
            if not gpu_engine_available():
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
        job.update(state="done", progress=1.0,
                   message=f"zone {'suivie' if track_it else 'fixe'} sur {span:.1f} s")
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


def privacy_overview():
    """État de l'analyse de confidentialité pour chaque clip du montage (revue)."""
    out = []
    for sid, clip in montage_items():
        entry = privacy.load(sid).get(clip["id"])
        tracks = privacy.all_tracks(entry)
        analyzed = bool(entry) and "key" in entry
        out.append({"sid": sid, "clip": clip["id"], "start": clip["start"], "end": clip["end"],
                    "analyzed": analyzed, "stale": analyzed and entry["key"] != privacy.view_key(clip),
                    "tracks": [{k: t.get(k) for k in ("id", "kind", "conf", "enabled", "thumb")} |
                               {"t0": t["samples"][0][0], "t1": t["samples"][-1][0]} for t in tracks if t["samples"]]})
    return out


def run_hyperlapse(sid, opts):
    """Résumé hyperlapse : toute la session en `duration` secondes, vitesse selon l'intérêt.

    Une vue fixe (celle de l'aperçu) + l'horizon choisi ; le moteur GPU ne décode que
    le nécessaire (saut aux images clés dans les portions rapides). Pas de son.
    """
    job = state["jobs"][sid]
    try:
        if not gpu_engine_available():
            raise ValueError("le résumé hyperlapse demande le moteur GPU (render/ + NVENC)")
        session, result = state["sessions"][sid]
        q = {**QUALITY["final"], "height": opts["height"], "crf": opts["crf"], "format": opts["format"],
             **FORMATS[opts["format"]]}
        out_w, out_h = output_size(q)
        name = f"hyperlapse_{out_h}p" if opts["format"] == "standard" else f"hyperlapse_{opts['format']}"
        cap = int(HYPERLAPSE_MBPS_1080 * 1e6 * out_w * out_h / (1920 * 1080))
        cap = min(cap, q.get("max_bitrate", cap))
        settings = get_settings()
        masks, tel_opts = settings["masks"], settings["telemetry"]
        detector = None
        if settings["privacy"]["enabled"]:
            # zones déjà analysées ou tracées dans les clips de la session, + détection image par image
            known = [t for e in privacy.load(sid).values() for t in privacy.all_tracks(e)]
        view = {"start": 0.0, "end": result["duration"], **opts["view"]}
        full = state["horizon"].get(sid, {})
        horizon_data = full.get("data") if full.get("status") == "done" else None
        if geometry.clip_horizon_mode(view) == "auto" and horizon_data is None:
            view["horizon"] = "fixe"  # analyse pas encore prête : on reste sur le support
        taus = hyperlapse.frame_times(result, opts["duration"])
        out_dir = EXPORTS / sid / name
        out_dir.mkdir(parents=True, exist_ok=True)
        job["engine"] = "GPU"
        files, done = [], 0
        for n, (seg, info) in enumerate(zip(session.segments, result["segments"])):
            src = seg.insv
            sel = taus[(taus >= info["offset"]) & (taus < info["offset"] + info["duration"] - 0.1)]
            if not len(sel) or not src:
                continue
            fps = source_fps(src)
            samples, first = np.unique(np.round((sel - info["offset"]) * float(fps)).astype(int), return_index=True)
            part_taus = sel[first]
            matrices, fovs = [], []
            for t in part_taus:
                v = geometry.clip_view_at(view, t)
                m = geometry.view_matrix(v["yaw"], v["pitch"], level_matrix_at(view, result, horizon_data, t), v["roll"])
                matrices.append([float(x) for x in m.flatten()])
                fovs.append(float(output_fov(v["fov"], out_w, out_h)))
            out = out_dir / f"part_{n:03d}.mp4"
            h264, spec = out.with_suffix(".h264"), out.with_suffix(".json")
            spec.write_text(json.dumps({
                "source": src, "start": 0.0, "duration": 0.0, "width": out_w, "height": out_h,
                "fov": fovs[0], "fovs": fovs, "cq": int(q["crf"]), "samples": [int(x) for x in samples],
                "max_bitrate": cap,
                "masks": [[m_["x"], m_["y"], m_["w"], m_["h"]] for m_ in masks],
                "matrices": matrices, "output": str(h264),
            }))
            job["message"] = f"fichier {n + 1}/{len(session.segments)} (GPU)"

            def progress(line, base=done):
                if line.startswith("frame="):
                    job["progress"] = min(1.0, (base + int(line[6:])) / len(taus))
            run_part_process(job, [str(RENDER_BIN), str(spec)], progress)
            run_part_process(job, ["ffmpeg", "-v", "error", "-y", "-framerate", str(fps), "-i", str(h264),
                                   "-c:v", "copy", "-movflags", "+faststart", str(out)], lambda _: None)
            h264.unlink(missing_ok=True)
            if settings["privacy"]["enabled"]:
                job["message"] = f"fichier {n + 1}/{len(session.segments)} : floutage visages et plaques"
                if detector is None:
                    detector = privacy.Detector()
                blurred = out.with_name(out.stem + "_flou.mp4")

                def blur_progress(f, base=done, count=len(sel)):
                    job["progress"] = min(1.0, (base + f * count) / len(taus))
                privacy.blur_video(out, blurred, [float(t) for t in part_taus],
                                   [np.array(m).reshape(3, 3) for m in matrices], fovs, known, out_w, out_h,
                                   encoder_args({**q, "max_bitrate": cap}), detector, blur_progress)
                blurred.replace(out)
            if tel_opts["enabled"] and result.get("gps_coverage", 0) > 0.3:
                job["message"] = f"fichier {n + 1}/{len(session.segments)} : télémétrie"
                out_t = np.arange(len(part_taus)) / float(fps)
                with_tel = out.with_name(out.stem + "_tel.mp4")
                if telemetry.overlay(out, with_tel, result, view, float(part_taus[0]), len(part_taus) / float(fps),
                                     out_w, out_h, tel_opts, first_part=not files, encoder_args=encoder_args(q),
                                     workdir=out_dir / f"tel_{n:03d}",
                                     time_map=lambda t, ot=out_t, pt=part_taus: np.interp(t, ot, pt)):
                    with_tel.replace(out)
            done += len(sel)
            files.append(out)
        if not files:
            raise ValueError("aucune image sélectionnée")
        job["message"] = "assemblage"
        listing = out_dir / "concat.txt"
        listing.write_text("".join(f"file '{f.name}'\n" for f in files))
        final = EXPORTS / f"{sid}_{name}.mp4"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
                        "-c", "copy", "-movflags", "+faststart", str(final)], check=True)
        job.update(state="done", progress=1.0, message="résumé terminé (GPU)", output=final.name)
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


# ---------------------------------------------------------------- HTTP

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        if not self.path.startswith(("/media/", "/api/export")):
            super().log_message(fmt, *args)

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"null")

    def _file(self, path, ctype=None):
        """Sert un fichier avec support des requêtes Range (lecture vidéo et navigation)."""
        size = path.stat().st_size
        ctype = ctype or mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        start, end = 0, size - 1
        m = re.match(r"bytes=(\d*)-(\d*)", self.headers.get("Range", ""))
        if m:
            if m.group(1):
                start = int(m.group(1))
                end = int(m.group(2)) if m.group(2) else end
            else:
                start = size - int(m.group(2))
            end = min(end, size - 1)
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        else:
            self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(end - start + 1))
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            left = end - start + 1
            try:
                while left > 0:
                    chunk = f.read(min(1 << 20, left))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    left -= len(chunk)
            except (BrokenPipeError, ConnectionResetError):
                pass  # le navigateur annule souvent les requêtes Range en cours

    def _media(self, name):
        for session, _ in state["sessions"].values():
            for seg in session.segments:
                if seg.lrv and Path(seg.lrv).name == name:
                    return self._file(Path(seg.lrv), "video/mp4")
        self.send_error(404)

    def _sources(self, body):
        """Ajoute ou retire un dossier de vidéos, puis relance l'analyse en arrière-plan."""
        if state["scan"].get("state") == "running":
            return self._json({"error": "analyse déjà en cours"}, 409)
        extra = json.loads(SOURCES.read_text()) if SOURCES.exists() else []
        if body.get("add"):
            folder = Path(str(body["add"])).expanduser()
            if not folder.is_dir():
                return self._json({"error": f"dossier introuvable : {folder}"}, 400)
            folder = str(folder.resolve())
            if not insta360.scan(folder):
                return self._json({"error": "aucune vidéo Insta360 (.insv/.lrv) dans ce dossier"}, 400)
            if folder not in extra and folder != state["dcim"]:
                extra.append(folder)
        elif body.get("remove"):
            extra = [f for f in extra if f != body["remove"]]
        analyze.DATA.mkdir(exist_ok=True)
        SOURCES.write_text(json.dumps(extra, indent=1))
        threading.Thread(target=rescan, daemon=True).start()
        return self._json({"ok": True})

    def _music_upload(self):
        """Reçoit une musique (corps brut) : POST /api/music?name=fichier.mp3."""
        qs = dict(x.split("=", 1) for x in self.path.partition("?")[2].split("&") if "=" in x)
        from urllib.parse import unquote
        name = Path(unquote(qs.get("name", ""))).name
        n = int(self.headers.get("Content-Length") or 0)
        if Path(name).suffix.lower() not in MUSIC_EXT or not name.strip("."):
            return self._json({"error": "format non pris en charge (mp3, m4a, aac, wav, ogg, opus, flac)"}, 400)
        if not 0 < n <= MUSIC_MAX_BYTES:
            return self._json({"error": "fichier vide ou trop gros (60 Mo max)"}, 400)
        MUSIC.mkdir(parents=True, exist_ok=True)
        (MUSIC / name).write_bytes(self.rfile.read(n))
        return self._json({"ok": True, "name": name})

    def _montage(self, body):
        """Montage : les clips du projet, dans l'ordre choisi, dans un seul export."""
        items = montage_items()
        if not items:
            return self._json({"error": "aucun clip dans le montage"}, 400)
        quality = body.get("quality", "final")
        if quality not in QUALITY:
            return self.send_error(400)
        if state["jobs"].get("montage", {}).get("state") == "running":
            return self._json({"error": "montage déjà en cours"}, 409)
        opts = export_opts(body, quality)
        state["jobs"]["montage"] = {"state": "running", "progress": 0.0, "quality": quality, "message": "démarrage",
                                    "format": opts.get("format", "standard")}
        threading.Thread(target=run_export, args=("montage", items, quality, opts), daemon=True).start()
        return self._json({"ok": True})

    def do_GET(self):
        path = self.path.split("?")[0]
        parts = path.strip("/").split("/")
        if path == "/":
            return self._file(UI / "index.html")
        if parts[0] == "ui" and len(parts) == 2 and (UI / parts[1]).is_file():
            return self._file(UI / parts[1])
        if parts[0] == "media" and len(parts) == 2:
            return self._media(parts[1])
        if parts[0] == "exports" and len(parts) == 2 and (EXPORTS / parts[1]).is_file():
            return self._file(EXPORTS / parts[1])
        if path == "/api/sessions":
            return self._json([{
                "id": r["id"], "date": r["date"], "time": r["time"], "duration": r["duration"],
                "gps_coverage": r["gps_coverage"], "candidates": len(r["candidates"]),
                "clips": len(get_selections(sid)), "clips_s": round(sum(c["end"] - c["start"] for c in get_selections(sid)), 1),
                "folder": r.get("folder"), "parts": r.get("parts", 1),
            } for sid, (_, r) in sorted(state["sessions"].items())])
        if path == "/api/project":
            proj = get_project()
            return self._json({**proj, "clips": [
                {"sid": sid, "id": c["id"], "start": c["start"], "end": c["end"], "excluded": ex, "auto": bool(c.get("auto")),
                 "yaw": c.get("yaw", 0), "pitch": c.get("pitch", 0), "fov": c.get("fov", 100),
                 "utc": state["sessions"][sid][1]["utc_t0"] + c["start"]}
                for sid, c, ex in montage_items(proj, with_excluded=True)]})
        if parts[0] == "thumb" and len(parts) == 2 and parts[1].removesuffix(".jpg") in state["sessions"]:
            sid = parts[1].removesuffix(".jpg")
            qs = dict(x.split("=", 1) for x in self.path.partition("?")[2].split("&") if "=" in x)
            try:
                num = lambda k, d: float(qs.get(k, d))
                img = thumbnail(sid, num("t", state["sessions"][sid][1]["duration"] / 3), num("yaw", 0),
                                num("pitch", -10), max(30.0, min(150.0, num("fov", 100))))
            except (ValueError, subprocess.SubprocessError):
                return self.send_error(500)
            return self._file(img, "image/jpeg")
        if parts == ["minimap.png"]:
            qs = dict(x.split("=", 1) for x in self.path.partition("?")[2].split("&") if "=" in x)
            if qs.get("sid") not in state["sessions"]:
                return self.send_error(404)
            try:
                img = minimap(qs["sid"], qs.get("clip"), max(160, min(800, int(qs.get("size", 320)))))
            except Exception:
                traceback.print_exc()
                return self.send_error(500)
            return self._file(img, "image/png")
        if path == "/api/privacy":
            return self._json({"job": {k: v for k, v in state["jobs"].get("privacy", {"state": "idle"}).items() if k != "proc"},
                               "enabled": get_settings()["privacy"]["enabled"], "clips": privacy_overview()})
        if parts[:2] == ["api", "privacy"] and len(parts) == 3 and parts[2] in state["sessions"]:
            data = privacy.load(parts[2])
            return self._json({cid: [t for t in privacy.all_tracks(e) if t.get("samples")] for cid, e in data.items()})
        if parts[0] == "privacy-thumb" and len(parts) == 2 and (privacy.THUMBS / parts[1]).is_file():
            return self._file(privacy.THUMBS / Path(parts[1]).name, "image/jpeg")
        if path == "/api/music":
            return self._json(music_files())
        if path == "/api/music/library":
            from urllib.parse import parse_qs
            qs = {k: v[0] for k, v in parse_qs(self.path.partition("?")[2]).items()}
            try:
                return self._json({"moods": musiclib.MOODS, "credits": musiclib.credits(),
                                   "pieces": musiclib.search(qs.get("q", ""), qs.get("mood", ""))})
            except Exception as e:
                return self._json({"error": f"catalogue indisponible : {e}"}, 502)
        if path == "/api/sources":
            return self._json({"scan": state["scan"], "folders": [
                {"path": f, "present": Path(f).is_dir(), "removable": f != state["dcim"],
                 "sessions": sum(1 for _, r in state["sessions"].values() if r.get("folder") == f)}
                for f in source_folders()]})
        if parts[:2] == ["api", "session"] and len(parts) == 3 and parts[2] in state["sessions"]:
            _, r = state["sessions"][parts[2]]
            return self._json({**r, "selections": get_selections(parts[2])})
        if path == "/api/settings":
            return self._json(get_settings())
        if parts[:2] == ["api", "horizon"] and len(parts) == 3 and parts[2] in state["sessions"]:
            h = request_horizon(parts[2], urgent=True)
            out = {k: v for k, v in h.items() if k != "data"}
            if h["status"] == "done":
                out.update(hz=h["data"]["hz"], up=h["data"]["up"], reliable=h["data"]["reliable"])
            return self._json(out)
        if parts[:2] == ["api", "export"] and len(parts) == 3:
            job = state["jobs"].get(parts[2], {"state": "idle"})
            return self._json({k: v for k, v in job.items() if k != "proc"})
        self.send_error(404)

    def do_PUT(self):
        parts = self.path.strip("/").split("/")
        if parts == ["api", "settings"]:
            cfg = self._body()
            current = get_settings()
            masks = [{k: min(1.0, max(0.0, float(m[k]))) for k in ("x", "y", "w", "h")}
                     for m in cfg.get("masks", current["masks"])]
            tel = {k: bool(v) for k, v in {**current["telemetry"], **cfg.get("telemetry", {})}.items()
                   if k in telemetry.DEFAULTS}
            priv = {"enabled": bool({**current["privacy"], **cfg.get("privacy", {})}["enabled"])}
            analyze.DATA.mkdir(exist_ok=True)
            SETTINGS.write_text(json.dumps({"masks": masks[:8], "telemetry": tel, "privacy": priv}, indent=1))
            return self._json({"ok": True})
        if parts[:2] == ["api", "privacy"] and len(parts) == 4 and parts[2] in state["sessions"]:
            body = self._body() or {}
            data = privacy.load(parts[2])
            entry = data.get(parts[3], {})
            if body.get("delete"):   # seules les zones tracées à la main se suppriment
                entry["manual"] = [t for t in entry.get("manual", []) if t["id"] != body.get("track")]
            for t in privacy.all_tracks(entry):
                if body.get("track") in (t["id"], "all"):
                    t["enabled"] = bool(body.get("enabled"))
            privacy.save(parts[2], data)
            return self._json({"ok": True})
        if parts == ["api", "project"]:
            body = self._body() or {}
            proj = get_project()
            for k in ("sessions", "order", "excluded"):
                if isinstance(body.get(k), list):
                    proj[k] = body[k]
            if isinstance(body.get("style"), dict):
                proj["style"] = finishing.clean({**proj["style"], **body["style"]})
            analyze.DATA.mkdir(exist_ok=True)
            PROJECT.write_text(json.dumps(proj, indent=1))
            return self._json({"ok": True})
        if parts[:2] == ["api", "selections"] and len(parts) == 3 and parts[2] in state["sessions"]:
            clips = self._body()
            for c in clips:
                c.setdefault("id", secrets.token_hex(4))
            SELECTIONS.mkdir(parents=True, exist_ok=True)
            selections_path(parts[2]).write_text(json.dumps(clips, indent=1))
            return self._json({"ok": True, "count": len(clips)})
        self.send_error(404)

    def do_POST(self):
        parts = self.path.strip("/").split("/")
        if parts == ["api", "sources"]:
            return self._sources(self._body() or {})
        if parts == ["api", "music", "library"]:
            body = self._body() or {}
            try:
                name = musiclib.download(str(body.get("filename", "")))
            except Exception as e:
                return self._json({"error": f"téléchargement impossible : {e}"}, 502)
            return self._json({"ok": True, "name": name})
        if parts[:2] == ["api", "music"]:
            return self._music_upload()
        if parts == ["api", "montage"]:
            return self._montage(self._body() or {})
        if parts == ["api", "automontage"]:
            body = self._body() or {}
            sids = [x for x in body.get("sids") or get_project()["sessions"] if x in state["sessions"]]
            target = max(20.0, min(900.0, float(body.get("duration", 120))))
            with lock:
                removed = 0
                existing = {}
                for sid in sids:   # les clips auto précédents sont remplacés ; les tiens restent
                    clips = get_selections(sid)
                    keep = [c for c in clips if not c.get("auto")]
                    if len(keep) != len(clips):
                        removed += len(clips) - len(keep)
                        selections_path(sid).write_text(json.dumps(keep, indent=1))
                    existing[sid] = keep
                if body.get("clear"):
                    return self._json({"removed": removed, "added": 0})
                style = get_project()["style"]
                plan = automontage.plan({sid: state["sessions"][sid][1] for sid in sids}, target, existing,
                                        style["duration"] if style["transition"] != "aucune" else 0.0)
                SELECTIONS.mkdir(parents=True, exist_ok=True)
                for sid, clips in plan.items():
                    selections_path(sid).write_text(json.dumps(sorted(existing[sid] + clips, key=lambda c: c["start"]), indent=1))
            added = [c for v in plan.values() for c in v]
            return self._json({"removed": removed, "added": len(added),
                               "seconds": round(sum(c["end"] - c["start"] for c in added), 1)})
        if parts == ["api", "follow"]:
            body = self._body() or {}
            sid = body.get("sid")
            clip = next((c for c in get_selections(sid) if c["id"] == body.get("clip")), None) \
                if sid in state["sessions"] else None
            if clip is None:
                return self._json({"error": "place la tête de lecture dans un clip"}, 400)
            if state["jobs"].get("follow", {}).get("state") == "running":
                return self._json({"error": "un suivi est déjà en cours"}, 409)
            try:
                v = body["view"]
                M = geometry.view_matrix(float(v["yaw"]), float(v["pitch"]),
                                         np.array(v["level"], float).reshape(3, 3), float(v.get("roll", 0)))
                x, y, w, h = (float(q) for q in body["box"])
                W, H = 1000 * float(body["aspect"]), 1000.0
                d, ax, ay = privacy.box_to_sphere((x * W, y * H, w * W, h * H), M, float(v["fov"]), W, H)
                view0 = {k: float(v.get(k, 0)) for k in ("yaw", "pitch", "roll", "fov")}
            except (KeyError, TypeError, ValueError):
                return self._json({"error": "zone invalide"}, 400)
            state["jobs"]["follow"] = {"state": "running", "progress": 0.0, "message": "démarrage du suivi"}
            threading.Thread(target=run_follow, daemon=True,
                             args=(sid, clip["id"], float(body["t"]), d.tolist(), ax, ay, view0)).start()
            return self._json({"ok": True})
        if parts == ["api", "privacy", "manual"]:
            body = self._body() or {}
            sid = body.get("sid")
            clip = next((c for c in get_selections(sid) if c["id"] == body.get("clip")), None) \
                if sid in state["sessions"] else None
            if clip is None:
                return self._json({"error": "place la tête de lecture dans un clip"}, 400)
            if state["jobs"].get("privacy", {}).get("state") == "running":
                return self._json({"error": "une analyse est déjà en cours"}, 409)
            try:
                v = body["view"]
                M = geometry.view_matrix(float(v["yaw"]), float(v["pitch"]),
                                         np.array(v["level"], float).reshape(3, 3), float(v.get("roll", 0)))
                x, y, w, h = (float(q) for q in body["box"])
                W, H = 1000 * float(body["aspect"]), 1000.0
                d, ax, ay = privacy.box_to_sphere((x * W, y * H, w * W, h * H), M, float(v["fov"]), W, H)
            except (KeyError, TypeError, ValueError):
                return self._json({"error": "zone invalide"}, 400)
            state["jobs"]["privacy"] = {"state": "running", "progress": 0.0, "message": "zone tracée"}
            threading.Thread(target=run_manual_zone, daemon=True,
                             args=(sid, clip, float(body["t"]), d.tolist(), ax, ay, bool(body.get("follow", True)))).start()
            return self._json({"ok": True})
        if parts == ["api", "privacy", "analyze"]:
            body = self._body() or {}
            if state["jobs"].get("privacy", {}).get("state") == "running":
                return self._json({"error": "analyse déjà en cours"}, 409)
            if body.get("sid") in state["sessions"] and body.get("clip"):
                items = [(body["sid"], c) for c in get_selections(body["sid"]) if c["id"] == body["clip"]]
            else:
                items = montage_items()
            if not items:
                return self._json({"error": "aucun clip à analyser"}, 400)
            state["jobs"]["privacy"] = {"state": "running", "progress": 0.0, "message": "démarrage"}
            threading.Thread(target=run_privacy, args=(items, bool(body.get("force"))), daemon=True).start()
            return self._json({"ok": True})
        if len(parts) == 3 and parts[:2] == ["api", "export"] and parts[2] in ("montage", "privacy", "follow") \
                and (self._body() or {}).get("cancel"):
            job = state["jobs"].get(parts[2], {})
            job["cancelled"] = True
            if job.get("proc"):
                job["proc"].terminate()
            return self._json({"ok": True})
        if len(parts) != 3 or parts[2] not in state["sessions"]:
            return self.send_error(404)
        sid = parts[2]
        if parts[:2] == ["api", "offset"]:
            offset = self._body().get("offset_s")
            with lock:
                ov = json.loads(analyze.OVERRIDES.read_text()) if analyze.OVERRIDES.exists() else {}
                if offset is None:
                    ov.pop(sid, None)
                else:
                    ov[sid] = round(float(offset), 2)
                analyze.OVERRIDES.write_text(json.dumps(ov, indent=1))
                session, _ = state["sessions"][sid]
                refs = [(r["utc_t0"], r["offset_s"]) for s2, (_, r) in state["sessions"].items()
                        if s2 != sid and r["offset_source"] in ("manuel", "corrélation")]
                old = state["sessions"][sid][1]
                r = {**analyze.analyze(session, ov, refs), "folder": old.get("folder"), "parts": old.get("parts", 1)}
                state["sessions"][sid] = (session, r)
            return self._json({**r, "selections": get_selections(sid)})
        if parts[:2] == ["api", "export"]:
            body = self._body() or {}
            if body.get("cancel"):
                job = state["jobs"].get(sid, {})
                job["cancelled"] = True
                if job.get("proc"):
                    job["proc"].terminate()
                return self._json({"ok": True})
            quality = body.get("quality", "preview")
            if quality not in QUALITY:
                return self.send_error(400)
            opts = export_opts(body, quality)
            if state["jobs"].get(sid, {}).get("state") == "running":
                return self._json({"error": "export déjà en cours"}, 409)
            state["jobs"][sid] = {"state": "running", "progress": 0.0, "quality": quality, "message": "démarrage",
                                  "format": opts.get("format", "standard")}
            clips = [(sid, c) for c in get_selections(sid)]
            threading.Thread(target=run_export, args=(sid, clips, quality, opts), daemon=True).start()
            return self._json({"ok": True})
        if parts[:2] == ["api", "hyperlapse"]:
            body = self._body() or {}
            duration = max(30.0, min(900.0, float(body.get("duration", 180))))
            if body.get("preview"):
                _, r = state["sessions"][sid]
                return self._json(hyperlapse.summary(r, duration))
            if state["jobs"].get(sid, {}).get("state") == "running":
                return self._json({"error": "export déjà en cours"}, 409)
            height = int(body.get("height", 1080))
            view = {k: float(body.get(k, d)) for k, d in (("yaw", 0), ("pitch", 0), ("roll", 0), ("fov", 100))}
            view["horizon"] = body.get("horizon") if body.get("horizon") in ("auto", "fixe", "aucun") else "fixe"
            opts = {"duration": duration, "height": height if height in HEIGHTS else 1080, "view": view,
                    "format": body.get("format") if body.get("format") in FORMATS else "standard",
                    "crf": max(14, min(28, int(body.get("crf", QUALITY["final"]["crf"]))))}
            state["jobs"][sid] = {"state": "running", "progress": 0.0, "quality": "hyperlapse", "message": "démarrage"}
            threading.Thread(target=run_hyperlapse, args=(sid, opts), daemon=True).start()
            return self._json({"ok": True})
        self.send_error(404)


def export_opts(body, quality):
    """Options d'export validées (qualité finale : résolution, qualité, destination)."""
    opts = {}
    if quality == "final":
        if int(body.get("height", 1080)) in HEIGHTS:
            opts["height"] = int(body.get("height", 1080))
        opts["crf"] = max(14, min(28, int(body.get("crf", QUALITY["final"]["crf"]))))
        opts["format"] = body.get("format") if body.get("format") in FORMATS else "standard"
    return opts


def main():
    p = argparse.ArgumentParser()
    p.add_argument("dcim", nargs="?", default=analyze.DEFAULT_DCIM)
    p.add_argument("--port", type=int, default=8360)
    p.add_argument("--host", default="127.0.0.1",
                   help="adresse d'écoute (ex. l'IP Wi-Fi pour un téléphone ; pas d'authentification)")
    a = p.parse_args()
    state["dcim"] = a.dcim
    nvenc_available()
    print("Analyse des sessions…")
    load_sessions()
    for _ in range(HORIZON_WORKERS):
        threading.Thread(target=horizon_worker, daemon=True).start()
    for sid, (_, r) in sorted(state["sessions"].items(), key=lambda kv: -kv[1][1]["duration"]):
        request_horizon(sid)
    srv = ThreadingHTTPServer((a.host, a.port), Handler)
    print(f"→ http://{a.host}:{a.port}/")
    srv.serve_forever()


if __name__ == "__main__":
    main()
