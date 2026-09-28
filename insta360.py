"""Lecture des fichiers Insta360 X5 (.insv / .lrv).

Le trailer Insta360 (atome `inst` en fin de fichier, terminé par MAGIC) contient
un index d'enregistrements. Ceux utilisés ici :
    0x0101  métadonnées (protobuf) : first_frame_timestamp (champ 24, µs horloge IMU)
            et gyro_timestamp (champ 28, ms) donnent le calage exact IMU ↔ vidéo
    0x0003  IMU à 1 kHz : u64 timestamp µs + 6 × u16 offset-binary (acc xyz, gyro xyz)
Les .lrv (proxys) portent le même trailer que les .insv. Référence de décodage :
telemetry-parser (Gyroflow), src/insta360/.
"""
import re
import struct
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

MAGIC = b"8db42d694ccc418790edff439fe026bf"
ACC_LSB_PER_G = 1024.0  # acc_range = 32 g (métadonnées)

IMU_DTYPE = np.dtype([("ts", "<u8"), ("v", "<u2", 6)])

NAME_RE = re.compile(r"^(VID|LRV)_(\d{8})_(\d{6})_(\d{2})_(\d{3})\.(insv|lrv)$", re.I)


def read_records(path):
    """Retourne {id: bytes} pour les enregistrements du trailer, ou {} s'il est absent."""
    with open(path, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        f.seek(size - 78)
        tail = f.read(78)
        if tail[-32:] != MAGIC:
            return {}
        start = size - struct.unpack("<I", tail[38:42])[0]
        f.seek(size - 78 - 310)
        index = f.read(310)
        out = {}
        for i in range(10, 310, 10):
            rid, length, offset = struct.unpack("<HII", index[i:i + 10])
            if length and rid in (0x03, 0x0101):
                f.seek(start + offset)
                out[rid] = f.read(length)
        return out


def _protobuf_fields(b):
    """Champs de premier niveau d'un message protobuf : {numéro: [(type, valeur)]}."""
    i, out = 0, {}

    def varint():
        nonlocal i
        v = s = 0
        while True:
            c = b[i]
            i += 1
            v |= (c & 0x7F) << s
            s += 7
            if c < 0x80:
                return v
    while i < len(b):
        key = varint()
        tag, wire = key >> 3, key & 7
        if wire == 0:
            val = varint()
        elif wire == 1:
            val, i = b[i:i + 8], i + 8
        elif wire == 2:
            n = varint()
            val, i = b[i:i + n], i + n
        elif wire == 5:
            val, i = b[i:i + 4], i + 4
        else:
            break
        out.setdefault(tag, []).append((wire, val))
    return out


def read_imu(path):
    """(temps vidéo s [N], acc en g [N,3], gyro brut centré [N,3]).

    Le temps vidéo vient du calage exact des métadonnées : (ts − first_frame_timestamp)/1e6
    − gyro_timestamp/1000.
    """
    rec = read_records(path)
    meta = _protobuf_fields(rec[0x0101])
    first_frame = meta[24][0][1]
    gyro_ts = struct.unpack("<d", meta[28][0][1])[0] if 28 in meta else 0.0
    imu = np.frombuffer(rec[0x03], dtype=IMU_DTYPE)
    v = imu["v"].astype(np.float64) - 32768.0
    t = (imu["ts"].astype(np.int64) - first_frame) / 1e6 - gyro_ts / 1000.0
    return t, v[:, :3] / ACC_LSB_PER_G, v[:, 3:]


@dataclass
class Segment:
    index: int
    insv: str | None = None
    lrv: str | None = None
    offset: float = 0.0     # début du segment dans le temps de session (s)
    duration: float = 0.0


@dataclass
class Session:
    id: str                 # ex. VID_20260829_112347
    date: str               # YYYYMMDD (heure locale caméra)
    time: str               # HHMMSS
    segments: list[Segment] = field(default_factory=list)


def scan(dcim):
    """Regroupe les fichiers d'un dossier DCIM en sessions (même horodatage de départ)."""
    sessions = {}
    for p in sorted(Path(dcim).rglob("*")):
        m = NAME_RE.match(p.name)
        if not m:
            continue
        kind, date, time, _, idx, _ = m.groups()
        sid = f"VID_{date}_{time}"
        s = sessions.setdefault(sid, Session(sid, date, time))
        seg = next((x for x in s.segments if x.index == int(idx)), None)
        if seg is None:
            seg = Segment(int(idx))
            s.segments.append(seg)
        if kind.upper() == "VID":
            seg.insv = str(p)
        else:
            seg.lrv = str(p)
    for s in sessions.values():
        s.segments.sort(key=lambda x: x.index)
    return [s for s in sessions.values() if all(seg.lrv for seg in s.segments)]
