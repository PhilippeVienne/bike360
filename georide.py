#!/usr/bin/env python3
"""Client minimal de l'API GeoRide (https://api.georide.com).

Identifiants lus depuis ~/.config/insta-build/georide.env (chmod 600) :
    GEORIDE_EMAIL=...
    GEORIDE_PASSWORD=...
Le token obtenu est mis en cache dans ~/.config/insta-build/georide.token.

Usage :
    python3 georide.py trackers
    python3 georide.py positions 2026-08-29 2026-08-30 [--tracker ID] [-o fichier.json]
    python3 georide.py trips     2026-08-29 2026-08-30 [--tracker ID]
"""
import argparse
import json
import os
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

API = "https://api.georide.com"
CONF_DIR = Path.home() / ".config" / "insta-build"
ENV_FILE = CONF_DIR / "georide.env"
TOKEN_FILE = CONF_DIR / "georide.token"


def _request(method, path, token=None, params=None, body=None):
    url = API + path
    if params:
        url += "?" + urllib.parse.urlencode(params)
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def _load_env():
    if not ENV_FILE.exists():
        sys.exit(f"Crée {ENV_FILE} avec GEORIDE_EMAIL=... et GEORIDE_PASSWORD=... (chmod 600)")
    if ENV_FILE.stat().st_mode & (stat.S_IRWXG | stat.S_IRWXO):
        sys.exit(f"{ENV_FILE} est lisible par d'autres utilisateurs : chmod 600 {ENV_FILE}")
    env = {}
    for line in ENV_FILE.read_text().splitlines():
        if "=" in line and not line.lstrip().startswith("#"):
            k, v = line.split("=", 1)
            env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def _login():
    env = _load_env()
    account = _request("POST", "/user/login",
                       body={"email": env["GEORIDE_EMAIL"], "password": env["GEORIDE_PASSWORD"]})
    token = account["authToken"]
    CONF_DIR.mkdir(parents=True, exist_ok=True)
    TOKEN_FILE.write_text(token)
    TOKEN_FILE.chmod(0o600)
    return token


def _call(method, path, params=None):
    token = TOKEN_FILE.read_text().strip() if TOKEN_FILE.exists() else _login()
    try:
        return _request(method, path, token, params)
    except urllib.error.HTTPError as e:
        if e.code not in (401, 403):
            raise
        return _request(method, path, _login(), params)


def _tracker_id(arg):
    if arg:
        return arg
    trackers = _call("GET", "/user/trackers")
    if len(trackers) != 1:
        sys.exit("Plusieurs trackers : précise --tracker (voir `georide.py trackers`)")
    return trackers[0]["trackerId"]


def fetch_positions(start, end, tracker=None):
    """Positions GeoRide entre deux dates ISO (fixtime UTC ; attention, speed est en nœuds)."""
    return _call("GET", f"/tracker/{_tracker_id(tracker)}/trips/positions", {"from": start, "to": end})


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("trackers")
    for name in ("positions", "trips"):
        s = sub.add_parser(name)
        s.add_argument("start", help="date/heure de début (ISO, ex. 2026-08-29)")
        s.add_argument("end", help="date/heure de fin (ISO)")
        s.add_argument("--tracker")
        s.add_argument("-o", "--output", help="fichier JSON de sortie (sinon stdout)")
    a = p.parse_args()

    if a.cmd == "trackers":
        for t in _call("GET", "/user/trackers"):
            print(t.get("trackerId"), t.get("trackerName"), t.get("model", ""))
        return

    tid = _tracker_id(a.tracker)
    path = f"/tracker/{tid}/trips" + ("/positions" if a.cmd == "positions" else "")
    data = _call("GET", path, {"from": a.start, "to": a.end})
    out = json.dumps(data, indent=1, ensure_ascii=False)
    if a.output:
        Path(a.output).write_text(out)
        print(f"{len(data)} enregistrements → {a.output}", file=sys.stderr)
    else:
        print(out)


if __name__ == "__main__":
    main()
