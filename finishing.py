"""Finition d'un montage : transitions entre clips, titre d'ouverture, musique de fond.

Chaque clip arrive en un fichier (vidéo + son) ; ffmpeg les enchaîne (xfade/acrossfade,
ou concat sans transition), ajoute fondus d'ouverture/fermeture, titre et musique, puis
réencode le tout une seule fois.
"""
import json
import subprocess

from telemetry import FONT, FONT_BOLD, _escape

TRANSITIONS = {"aucune": None, "fondu": "fade", "noir": "fadeblack", "glisse": "slideleft"}
DEFAULTS = {"transition": "fondu", "duration": 0.6, "title": "", "subtitle": "",
            "music": "", "music_volume": 0.8, "original_volume": 0.35, "end_card": True}
END_CARD_S = 5.0


def clean(style):
    """Style validé (valeurs bornées, clés connues)."""
    s = {**DEFAULTS, **{k: v for k, v in (style or {}).items() if k in DEFAULTS}}
    s["transition"] = s["transition"] if s["transition"] in TRANSITIONS else DEFAULTS["transition"]
    s["duration"] = max(0.2, min(2.0, float(s["duration"])))
    s["music_volume"] = max(0.0, min(1.5, float(s["music_volume"])))
    s["original_volume"] = max(0.0, min(1.5, float(s["original_volume"])))
    s["title"], s["subtitle"], s["music"] = (str(s[k])[:120] for k in ("title", "subtitle", "music"))
    s["end_card"] = bool(s["end_card"])
    return s


def is_plain(style):
    """Rien à faire au-delà d'un simple assemblage (copie sans réencodage)."""
    return TRANSITIONS[style["transition"]] is None and not style["title"] and not style["music"] and not style["end_card"]


def _probe(path):
    out = json.loads(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration:stream=codec_type",
                                     "-of", "json", str(path)], capture_output=True, text=True).stdout)
    return float(out["format"]["duration"]), any(s["codec_type"] == "audio" for s in out.get("streams", []))


def finish(clip_files, final, style, encoder_args, W, H, music_path=None, audio_bitrate="160k", run=subprocess.run,
           end_card=None):
    """Assemble les clips (dans l'ordre) avec transitions, titre et musique → `final`.

    `end_card` : image de fin (PNG W×H) affichée END_CARD_S secondes après le dernier clip.
    """
    info = [_probe(f) for f in clip_files]
    if end_card:
        info.append((END_CARD_S, False))
    n = len(info)
    kind = TRANSITIONS[style["transition"]]
    if end_card and not kind:   # coupe franche entre les clips, mais fondu vers la carte de fin
        kind = "fade"
    T = min(style["duration"], min(d for d, _ in info) / 2) if kind and n > 1 else 0.0
    inputs, graph = [], []
    for k, (f, (d, has_audio)) in enumerate(zip([*clip_files, *([end_card] if end_card else [])], info)):
        if end_card and k == n - 1:
            inputs += ["-loop", "1", "-framerate", "30000/1001", "-t", f"{d:.3f}", "-i", str(f)]
        else:
            inputs += ["-i", str(f)]
        graph.append(f"[{k}:v]fps=30000/1001,format=yuv420p,setsar=1,settb=AVTB[v{k}]")
        if has_audio:
            graph.append(f"[{k}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a{k}]")
        else:  # clip sans son : silence de même durée, pour garder l'enchaînement audio
            graph.append(f"anullsrc=r=48000:cl=stereo,atrim=0:{d:.3f},aformat=sample_fmts=fltp[a{k}]")

    if T > 0:
        v, a, acc = "v0", "a0", info[0][0]
        for k in range(1, n):
            graph.append(f"[{v}][v{k}]xfade=transition={kind}:duration={T:.3f}:offset={acc - T:.3f}[x{k}]")
            graph.append(f"[{a}][a{k}]acrossfade=d={T:.3f}[y{k}]")
            v, a, acc = f"x{k}", f"y{k}", acc + info[k][0] - T
        total = acc
    else:
        graph.append("".join(f"[v{k}][a{k}]" for k in range(n)) + f"concat=n={n}:v=1:a=1[xc][yc]")
        v, a, total = "xc", "yc", sum(d for d, _ in info)

    # ouverture au noir, fermeture au noir (image et son)
    fin, fout = 0.8, min(1.5, total / 4)
    graph.append(f"[{v}]fade=t=in:st=0:d={fin},fade=t=out:st={total - fout:.3f}:d={fout:.3f}[vf]")
    graph.append(f"[{a}]afade=t=in:st=0:d={fin},afade=t=out:st={total - fout:.3f}:d={fout:.3f}[af]")
    v, a = "vf", "af"

    if style["title"]:
        U = min(W, H)
        alpha = "if(lt(t,0.8),0,if(lt(t,1.6),(t-0.8)/0.8,if(lt(t,4.4),1,max(0,(5.2-t)/0.8))))"
        lines = [(style["title"], FONT_BOLD, int(U * 0.085), 0.0)]
        if style["subtitle"]:
            lines.append((style["subtitle"], FONT, int(U * 0.042), U * 0.075))
        for k, (text, font, size, dy) in enumerate(lines):
            graph.append(f"[{v}]drawtext=fontfile={font}:text='{_escape(text)}':fontsize={size}:fontcolor=white"
                         f":alpha='{alpha}':shadowcolor=black@0.55:shadowx=3:shadowy=3"
                         f":x=(w-tw)/2:y=h*0.42-th/2+{dy:.0f}:enable='lt(t,5.3)'[t{k}]")
            v = f"t{k}"

    if music_path:
        m = n
        inputs += ["-stream_loop", "-1", "-i", str(music_path)]
        graph.append(f"[{m}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,"
                     f"atrim=0:{total:.3f},volume={style['music_volume']:.2f},"
                     f"afade=t=in:st=0:d=1.5,afade=t=out:st={max(0, total - 3):.3f}:d=3[mus]")
        graph.append(f"[{a}]volume={style['original_volume']:.2f}[orig]")
        graph.append("[orig][mus]amix=inputs=2:duration=first:normalize=0[am]")
        a = "am"

    run(["ffmpeg", "-v", "error", "-y", *inputs, "-filter_complex", ";".join(graph),
         "-map", f"[{v}]", "-map", f"[{a}]", *encoder_args, "-c:a", "aac", "-b:a", audio_bitrate,
         "-movflags", "+faststart", str(final)], check=True, capture_output=True, text=True)
    return total
