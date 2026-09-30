#!/bin/sh
# Installe le serveur Rust comme service utilisateur (démarrage automatique à la connexion).
#   sh packaging/install.sh            compile, installe, active le service
#   sh packaging/install.sh --no-start compile et installe sans démarrer
# Les données restent dans le dépôt (INSTA_BUILD_ROOT) : rien n'est déplacé.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
bin="$HOME/.local/bin"
conf="$HOME/.config/insta-build"
unit="$HOME/.config/systemd/user"

cargo build --release --manifest-path "$repo/Cargo.toml"
mkdir -p "$bin" "$conf" "$unit"
for b in insta-server insta-render insta-tool; do
    install -m 755 "$repo/target/release/$b" "$bin/$b"
done

if [ ! -f "$conf/server.env" ]; then
    cat > "$conf/server.env" <<ENV
# Réglages du service insta-build (relancer : systemctl --user restart insta-build)
INSTA_BUILD_ROOT=$repo
DCIM=/run/media/$USER/Insta360 X5/DCIM
HOST=127.0.0.1
PORT=8360
ENV
    chmod 600 "$conf/server.env"
    echo "Réglages créés : $conf/server.env (HOST=0.0.0.0 pour y accéder depuis le téléphone)"
fi
install -m 644 "$repo/packaging/insta-build.service" "$unit/insta-build.service"
systemctl --user daemon-reload

if [ "${1:-}" != "--no-start" ]; then
    systemctl --user enable --now insta-build.service
    echo "Service démarré : systemctl --user status insta-build"
fi
