#!/bin/bash
# Préparation de la machine de l'essai (lancée une fois par AWS au premier démarrage) : outils,
# montage S3 Files, compilation de Bike360. Journal : /var/log/bike360-preparation.log ;
# le fichier /opt/bike360/pret apparaît quand tout est en place.
exec > /var/log/bike360-preparation.log 2>&1
set -eux
export DEBIAN_FRONTEND=noninteractive HOME=/root

# l'image lance ses propres mises à jour au démarrage : on attend le verrou plutôt que d'échouer
apt-get -o DPkg::Lock::Timeout=600 update
apt-get -o DPkg::Lock::Timeout=600 install -y ffmpeg git build-essential pkg-config jq bc
curl -fsS https://amazon-efs-utils.aws.com/efs-utils-installer.sh | sh -s -- --install
curl -fsS https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env

mkdir -p /mnt/bike360 /opt/bike360
cat > /opt/bike360/reglages <<REGLAGES
FILE_SYSTEM=${file_system}
BUCKET=${bucket}
AWS_DEFAULT_REGION=${region}
REGLAGES
# la cible de montage peut mettre quelques minutes à répondre
for _ in $(seq 1 30); do mount -t s3files ${file_system}:/ /mnt/bike360 && break; sleep 20; done
mountpoint /mnt/bike360

git clone --depth 1 --branch ${branch} ${repo} /opt/bike360/code
cd /opt/bike360/code
export PATH=$PATH:/usr/local/cuda/bin   # nvcc, pour le moteur GPU
cargo build --release
cargo build --release -p bike360-render
touch /opt/bike360/pret
