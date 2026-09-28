#!/usr/bin/env python3
"""Serveur local de l'outil de tri : UI, streaming des proxys .lrv, sélections, export ffmpeg.

Usage : python3 server.py [DCIM] [--port 8360] [--host 127.0.0.1]
"""
import argparse
import json
import math
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
import geometry
import horizon
import hyperlapse
import insta360
import telemetry

ROOT = Path(__file__).resolve().parent
UI = ROOT / "ui"
SELECTIONS = analyze.DATA / "selections"
EXPORTS = ROOT / "exports"
SETTINGS = analyze.DATA / "settings.json"
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

state = {"dcim": None, "sessions": {}, "jobs": {}, "nvenc": None, "horizon": {}}
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


def load_sessions():
    results = analyze.analyze_all(state["dcim"])
    by_id = {s.id: s for s in insta360.scan(state["dcim"])}
    state["sessions"] = {sid: (by_id[sid], r) for sid, r in results.items()}


def selections_path(sid):
    return SELECTIONS / f"{sid}.json"


def get_settings():
    cfg = json.loads(SETTINGS.read_text()) if SETTINGS.exists() else {}
    return {"masks": cfg.get("masks", []), "telemetry": {**telemetry.DEFAULTS, **cfg.get("telemetry", {})}}


def get_selections(sid):
    p = selections_path(sid)
    return json.loads(p.read_text()) if p.exists() else []


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
    graph = v360_filter({**clip, "fov": targets[0][2]}, out_w, out_h, q["source"], masks, size, angles, cmdfile)
    cmd = ["ffmpeg", "-v", "error", "-y", "-ss", f"{ss:.3f}", "-t", f"{dur:.3f}", "-i", src,
           "-filter_complex", graph, "-map", "[v]", "-map", "0:a:0?", *encoder_args(q),
           "-c:a", "aac", "-b:a", q.get("audio", "160k"), "-ar", "48000", "-ac", "2",
           "-movflags", "+faststart", "-progress", "pipe:1", "-nostats", str(out)]

    def progress(line):
        if line.startswith("out_time_us=") and line[12:].strip().isdigit():
            report(int(line[12:]) / 1e6)
    run_part_process(job, cmd, progress)


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


def run_export(sid, quality, opts):
    job = state["jobs"][sid]
    try:
        session, result = state["sessions"][sid]
        q = {**QUALITY[quality], **opts, **FORMATS.get(opts.get("format", "standard"), {})}
        out_w, out_h = output_size(q)
        name = f"{quality}_{out_h}p" if q.get("format", "standard") == "standard" else q["format"]
        clips = get_selections(sid)
        settings = get_settings()
        masks, tel_opts = settings["masks"], settings["telemetry"]
        sizes = {}
        if not clips:
            raise ValueError("aucun clip sélectionné")
        total_s = sum(c["end"] - c["start"] for c in clips)
        if q.get("format") == "vertical" and total_s > 90:
            job["warning"] = f"{int(total_s // 60)} min {int(total_s % 60):02d} : long pour un Reel (90 s) ou un Short (60 s)"
        gpu = q["source"] == "insv" and gpu_engine_available()
        # Horizon : analyse complète si déjà prête, sinon seulement les portions des clips.
        full = state["horizon"].get(sid, {})
        full_data = full.get("data") if full.get("status") == "done" else None
        job["engine"] = "GPU" if gpu else "ffmpeg"
        out_dir = EXPORTS / sid / name
        out_dir.mkdir(parents=True, exist_ok=True)
        todo = [(i, c, p) for i, c in enumerate(clips) for p in clip_parts(session, result, c)]
        total = sum(p[2] for _, _, p in todo)
        done, files = 0.0, []
        clip_horizon = {}
        for n, (i, clip, (seg, ss, dur)) in enumerate(todo):
            horizon_data = full_data
            if geometry.clip_horizon_mode(clip) == "auto" and full_data is None:
                if i not in clip_horizon:
                    job["message"] = f"horizon du clip {i + 1}/{len(clips)}"
                    clip_horizon[i] = horizon.compute_range(session, result, clip["start"] - 3, clip["end"] + 3)
                horizon_data = clip_horizon[i]
            src = seg.insv if q["source"] == "insv" else seg.lrv
            if not src:
                raise ValueError(f"fichier {q['source']} manquant pour le segment {seg.index}")
            out = out_dir / f"part_{n:03d}.mp4"
            job["message"] = f"clip {i + 1}/{len(clips)} ({job['engine']})"

            def report(t, base=done):
                job["progress"] = min(1.0, (base + t) / total)
            if gpu:
                export_part_gpu(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, out_w, out_h, out, report)
            else:
                if src not in sizes:
                    sizes[src] = source_size(src, q["source"])
                export_part_ffmpeg(job, clip, result, horizon_data, seg, ss, dur, src, q, masks, sizes[src], out_w, out_h, out, report)
            if tel_opts["enabled"] and result.get("gps_coverage", 0) > 0.3:
                job["message"] = f"clip {i + 1}/{len(clips)} : télémétrie"
                seg_offset = next(x["offset"] for x in result["segments"] if x["index"] == seg.index)
                with_tel = out.with_name(out.stem + "_tel.mp4")
                if telemetry.overlay(out, with_tel, result, clip, seg_offset + ss, dur, out_w, out_h, tel_opts,
                                     first_part=abs(seg_offset + ss - clip["start"]) < 0.5,
                                     encoder_args=encoder_args(q), workdir=out_dir / f"tel_{n:03d}"):
                    with_tel.replace(out)
            done += dur
            files.append(out)
        job["message"] = "assemblage"
        listing = out_dir / "concat.txt"
        listing.write_text("".join(f"file '{f.name}'\n" for f in files))
        final = EXPORTS / f"{sid}_{name}.mp4"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
                        "-c", "copy", "-movflags", "+faststart", str(final)], check=True)
        job.update(state="done", progress=1.0, message=f"terminé ({job['engine']})", output=final.name)
    except Exception as e:
        traceback.print_exc()
        job.update(state="error", message=str(e))
    finally:
        job.pop("proc", None)


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
                "clips": len(get_selections(sid)),
            } for sid, (_, r) in sorted(state["sessions"].items())])
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
            analyze.DATA.mkdir(exist_ok=True)
            SETTINGS.write_text(json.dumps({"masks": masks[:8], "telemetry": tel}, indent=1))
            return self._json({"ok": True})
        if parts[:2] == ["api", "selections"] and len(parts) == 3 and parts[2] in state["sessions"]:
            clips = self._body()
            SELECTIONS.mkdir(parents=True, exist_ok=True)
            selections_path(parts[2]).write_text(json.dumps(clips, indent=1))
            return self._json({"ok": True, "count": len(clips)})
        self.send_error(404)

    def do_POST(self):
        parts = self.path.strip("/").split("/")
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
                r = analyze.analyze(session, ov, refs)
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
            opts = {}
            if quality == "final":
                if int(body.get("height", 1080)) in HEIGHTS:
                    opts["height"] = int(body.get("height", 1080))
                opts["crf"] = max(14, min(28, int(body.get("crf", QUALITY["final"]["crf"]))))
                opts["format"] = body.get("format") if body.get("format") in FORMATS else "standard"
            if state["jobs"].get(sid, {}).get("state") == "running":
                return self._json({"error": "export déjà en cours"}, 409)
            state["jobs"][sid] = {"state": "running", "progress": 0.0, "quality": quality, "message": "démarrage",
                                  "format": opts.get("format", "standard")}
            threading.Thread(target=run_export, args=(sid, quality, opts), daemon=True).start()
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
