#!/bin/sh
# Essai local du module Envoi sur l'émulateur floci : service d'envoi + code du navigateur.
#   sh cloud/essai-envoi.sh APERÇU.lrv
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
lrv=${1:-}
port=8371
export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_DEFAULT_REGION=eu-west-3
export AWS_ENDPOINT_URL=http://127.0.0.1:4566
export AWS_PAGER=""
[ -n "$lrv" ] && [ -f "$lrv" ] || { echo "donner un fichier .lrv : sh cloud/essai-envoi.sh /chemin/LRV_….lrv" >&2; exit 1; }

echo "== Émulateur, infrastructure et service d'envoi"
docker compose -f "$repo/cloud/compose.yml" up -d >/dev/null 2>&1
for _ in $(seq 1 30); do aws s3api list-buckets >/dev/null 2>&1 && break; sleep 1; done
(cd "$repo/cloud/terraform" && terraform init -input=false >/dev/null && terraform apply -auto-approve -input=false -var local=true >/dev/null)
bucket=$(cd "$repo/cloud/terraform" && terraform output -raw bucket)
cargo build --release --manifest-path "$repo/cloud/envoi/Cargo.toml" 2>/dev/null
name=$(basename "$lrv")
aws s3 rm "s3://$bucket/apercus/essai/$name" >/dev/null 2>&1 || true
# morceaux de 8 Mo pour qu'un petit aperçu en compte plusieurs
BIKE360_PART_MB=8 "$repo/cloud/envoi/target/release/bike360-envoi" --bucket "$bucket" --client essai --ui "$repo/ui" --port "$port" > /dev/null 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
for _ in $(seq 1 30); do curl -sf -o /dev/null "http://127.0.0.1:$port/api/envoi/rushs" && break; sleep 1; done
echo "   ✓ service d'envoi sur le port $port, compartiment $bucket"

echo "== Envoi par le code du navigateur"
node "$repo/cloud/essai-envoi.mjs" "http://127.0.0.1:$port" "$lrv"

echo "== Contrôle dans le stockage"
aws s3api get-object --bucket "$bucket" --key "apercus/essai/$name" "$repo/cloud/essai-retour.lrv" >/dev/null
cmp -s "$lrv" "$repo/cloud/essai-retour.lrv" && echo "   ✓ fichier assemblé identique à l'original" || { echo "   ✗ le fichier assemblé diffère" >&2; exit 1; }
rm -f "$repo/cloud/essai-retour.lrv"
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/ui/envoi.html")
[ "$code" = 200 ] && echo "   ✓ page d'envoi servie" || { echo "   ✗ page d'envoi : $code" >&2; exit 1; }
echo "Essai réussi."
