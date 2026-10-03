#!/bin/sh
# Installe le serveur Rust comme service utilisateur (démarrage automatique à la connexion).
#   sh packaging/install.sh            compile, installe, active le service
#   sh packaging/install.sh --no-start compile et installe sans démarrer
# Les données restent dans le dépôt (BIKE360_ROOT) : rien n'est déplacé.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
bin="$HOME/.local/bin"
conf="$HOME/.config/bike360"
unit="$HOME/.config/systemd/user"

cargo build --release --manifest-path "$repo/Cargo.toml"
# moteur GPU (NVDEC/CUDA/NVENC) : seulement si le kit CUDA est là ; sinon exports par ffmpeg (processeur)
if command -v nvcc >/dev/null 2>&1; then
    cargo build --release --manifest-path "$repo/Cargo.toml" -p bike360-render
else
    echo "nvcc introuvable : moteur GPU non compilé (exports ffmpeg ; pas de floutage, suivi ni hyperlapse)."
fi
mkdir -p "$bin" "$conf" "$unit"
for b in bike360-server bike360-render bike360-tool; do
    [ -f "$repo/target/release/$b" ] && install -m 755 "$repo/target/release/$b" "$bin/$b"
done

if [ ! -f "$conf/server.env" ]; then
    cat > "$conf/server.env" <<ENV
# Réglages du service bike360 (relancer : systemctl --user restart bike360)
BIKE360_ROOT=$repo
DCIM=/run/media/$USER/Insta360 X5/DCIM
HOST=127.0.0.1
PORT=8360
# mot de passe de l'interface (vide = accès libre, à éviter hors de 127.0.0.1)
BIKE360_PASSWORD=$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | cut -c1-20)
ENV
    chmod 600 "$conf/server.env"
    echo "Réglages créés : $conf/server.env (HOST=0.0.0.0 pour y accéder depuis le téléphone)"
fi
install -m 644 "$repo/packaging/bike360.service" "$unit/bike360.service"
systemctl --user daemon-reload

if [ "${1:-}" != "--no-start" ]; then
    systemctl --user enable --now bike360.service
    echo "Service démarré : systemctl --user status bike360"
fi
