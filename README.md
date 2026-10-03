# Bike360

Tri et montage des balades à moto filmées avec une **Insta360 X5** (vidéo 360°). On repère les
bons moments, on cadre la vue, on assemble les clips avec musique, on floute visages et plaques,
puis on exporte une vidéo plate (YouTube, Reels, carré…). Tout tourne en local, dans le navigateur.

## Fonctions

- **Analyse des sessions** : profil seconde par seconde (IMU, GPS GeoRide si disponible),
  moments forts, statistiques, horizon mesuré dans l'image, inclinaison de la moto.
- **Repérage et coupe** : frise, clips, cadrage de la vue 360° avec points clés, accélérés.
- **Éditeur de montage** : clips en blocs sur une frise, pistes audio multiples (volume, fondus,
  rognage, forme d'onde), écoute du mixage, transitions, titre, carte de fin avec statistiques.
- **Confidentialité** : floutage des visages et des plaques (détection, suivi, zones manuelles).
- **Export** : moteur GPU (NVDEC → CUDA → NVENC) ou ffmpeg, incrustations (vitesse, mini-carte,
  altitude, inclinaison), chapitres YouTube.
- **Dossiers et cartes SD** : détection des cartes, navigateur de dossiers avec aperçu, analyse
  automatique des nouveaux fichiers.
- **Deux interfaces** : ordinateur et téléphone, avec accès protégé par mot de passe.

## Prérequis

- Linux, [Rust](https://rustup.rs) (édition 2024), `ffmpeg` et `ffprobe`.
- Moteur GPU (facultatif mais ~5× plus rapide) : carte NVIDIA avec NVENC et kit CUDA (`nvcc`).
- Floutage : `onnxruntime` (`pip install onnxruntime-gpu`). Les modèles ne sont pas dans ce dépôt :
  plaques et véhicules sont téléchargés au premier usage (open-image-models) ; pour les visages et
  le suivi, placer `face_detection_yunet_2023mar.onnx` et `object_tracking_vittrack_2023sep.onnx`
  ([OpenCV Zoo](https://github.com/opencv/opencv_zoo)) dans `data/cache/models/`.

## Installation

```sh
sh packaging/install.sh
```

Compile, installe dans `~/.local/bin` et active un service utilisateur systemd. Les réglages
sont dans `~/.config/bike360/server.env` (dossier de la carte, adresse, port, mot de passe).
L'interface est ensuite sur <http://127.0.0.1:8360>.

Sans service : `cargo build --release`, puis
`BIKE360_PASSWORD=… target/release/bike360-server "/chemin/vers/DCIM" --host 127.0.0.1 --port 8360`.

## Sécurité

- Le serveur n'a **pas de chiffrement** propre. Pour l'ouvrir hors de la machine, mettez-le
  derrière HTTPS (par exemple `tailscale serve`) et **définissez `BIKE360_PASSWORD`** : une page de
  connexion et un cookie signé protègent alors l'interface et l'API. Sans mot de passe, ne l'écoutez
  que sur `127.0.0.1`.
- Les données personnelles (positions GPS, sélections, exports) restent dans `data/` et
  `exports/`, ignorés par git.

## Organisation

| Dossier | Rôle |
| --- | --- |
| `core/` | analyse, géométrie, horizon, finition, confidentialité (bibliothèque Rust + tests) |
| `server/` | serveur HTTP, API, exports |
| `render/` | moteur GPU (NVDEC, noyaux CUDA, NVENC) |
| `ui/` | interface web (modules ES, sans dépendance de build) |
| `packaging/` | service systemd et script d'installation |
| `*.py` | version Python d'origine, gardée comme référence des tests de non-régression |

Tests : `cargo test`.

## Crédits et données tierces

- Cartes © contributeurs [OpenStreetMap](https://www.openstreetmap.org/copyright).
- Musiques libres proposées par l'application : Kevin MacLeod ([incompetech.com](https://incompetech.com)),
  licence CC BY 4.0 — le crédit est ajouté automatiquement à la fin du montage.
- Détection : modèles d'[open-image-models](https://github.com/ankandrew/open-image-models)
  (plaques, véhicules) et YuNet / VitTrack ([OpenCV Zoo](https://github.com/opencv/opencv_zoo)),
  obtenus séparément et soumis à leurs propres licences.
- Positions GPS : API [GeoRide](https://georide.com) (compte personnel requis, facultatif).

Projet indépendant, non affilié à Insta360 ni à GeoRide.

## Licence

[EUPL-1.2](LICENSE) — European Union Public Licence v. 1.2.
