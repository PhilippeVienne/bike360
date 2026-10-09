#!/bin/sh
# Essai local du service commun sur l'émulateur floci : comptes, Envoi, analyse à l'arrivée, Bibliothèque.
#   sh cloud/essai-envoi.sh APERÇU.lrv
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
lrv=${1:-}
port=8371
atelier_port=8398
work="$repo/cloud/essai"
export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_DEFAULT_REGION=eu-west-3
export AWS_ENDPOINT_URL=http://127.0.0.1:4566
export AWS_PAGER=""
[ -n "$lrv" ] && [ -f "$lrv" ] || { echo "donner un fichier .lrv : sh cloud/essai-envoi.sh /chemin/LRV_….lrv" >&2; exit 1; }

echo "== Émulateur, infrastructure et service d'envoi"
docker compose -f "$repo/cloud/compose.yml" up -d >/dev/null 2>&1
for _ in $(seq 1 30); do aws s3api list-buckets >/dev/null 2>&1 && break; sleep 1; done
(cd "$repo/cloud/terraform" && terraform init -input=false >/dev/null && terraform apply -auto-approve -input=false -var local=true >/dev/null)
bucket=$(cd "$repo/cloud/terraform" && terraform output -raw bucket)
table=$(cd "$repo/cloud/terraform" && terraform output -raw table)
queue=$(cd "$repo/cloud/terraform" && terraform output -raw file_gpu)
pool=$(cd "$repo/cloud/terraform" && terraform output -raw pool)
issuer=$(cd "$repo/cloud/terraform" && terraform output -raw issuer)
app_client=$(cd "$repo/cloud/terraform" && terraform output -raw app_client)
aws sqs purge-queue --queue-url "$queue" >/dev/null 2>&1 || true
cargo build --release --manifest-path "$repo/cloud/service/Cargo.toml" 2>/dev/null
name=$(basename "$lrv")
# un atelier en mode hébergé, sans rush, pour constater qu'il est prévenu à l'arrivée d'un aperçu
rm -rf "$work"; mkdir -p "$work/rushs" "$work/root"
HOME="$work" BIKE360_CLOUD=1 BIKE360_PASSWORD= BIKE360_ROOT="$work/root" \
    "$repo/target/release/bike360-server" "$work/rushs" --host 127.0.0.1 --port "$atelier_port" > "$work/atelier.log" 2>&1 &
atelier=$!
# morceaux de 8 Mo pour qu'un petit aperçu en compte plusieurs
# garde en corbeille nulle : l'essai vide la corbeille tout de suite
BIKE360_TRASH_DAYS=0 BIKE360_PART_MB=8 "$repo/cloud/service/target/release/bike360-envoi" --bucket "$bucket" --table "$table" --queue "$queue" \
    --issuer "$issuer" --app-client "$app_client" \
    --atelier "http://127.0.0.1:$atelier_port" --ui "$repo/ui" --port "$port" > "$work/envoi.log" 2>&1 &
server=$!
trap 'kill "$server" "$atelier" 2>/dev/null || true' EXIT
for _ in $(seq 1 30); do curl -sf -o /dev/null "http://127.0.0.1:$port/api/compte" && break; sleep 1; done
for _ in $(seq 1 60); do curl -sf -o /dev/null "http://127.0.0.1:$atelier_port/api/sessions" && break; sleep 1; done   # l'atelier doit écouter avant le premier envoi
echo "   ✓ service d'envoi sur le port $port, compartiment $bucket"

echo "== Comptes"
node "$repo/cloud/essai-comptes.mjs" "http://127.0.0.1:$port" "$pool" "$work/jetons.json"
token() { node -e "console.log(JSON.parse(require('fs').readFileSync('$work/jetons.json'))['$1']['$2'])"; }
export BIKE360_TOKEN=$(token a token) BIKE360_TOKEN_B=$(token b token)
client=$(token a sub)

echo "== Envoi par le code du navigateur"
node "$repo/cloud/essai-envoi.mjs" "http://127.0.0.1:$port" "$lrv"

echo "== Contrôle dans le stockage"
aws s3api get-object --bucket "$bucket" --key "apercus/$client/$name" "$repo/cloud/essai-retour.lrv" >/dev/null
cmp -s "$lrv" "$repo/cloud/essai-retour.lrv" && echo "   ✓ fichier assemblé identique à l'original" || { echo "   ✗ le fichier assemblé diffère" >&2; exit 1; }
rm -f "$repo/cloud/essai-retour.lrv"
item=$(aws dynamodb get-item --table-name "$table" --key "{\"pk\":{\"S\":\"client#$client\"},\"sk\":{\"S\":\"rush#$name\"}}" \
    --query 'Item.[camera_modele.S, duree_s.N, nature.S, session.S]' --output text | sed -E 's/_[A-Z0-9]{4}$/_<caméra>/')
[ -n "$item" ] && [ "$item" != None ] && echo "   ✓ index : $item" || { echo "   ✗ rush absent de l'index" >&2; exit 1; }
grep -q '"POST /api/sources" 200' "$work/atelier.log" && echo "   ✓ atelier prévenu de l'arrivée de l'aperçu" || { echo "   ✗ atelier non prévenu" >&2; exit 1; }
for page in compte envoi bibliotheque; do
    code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/ui/$page.html")
    [ "$code" = 200 ] && echo "   ✓ page $page servie" || { echo "   ✗ page $page : $code" >&2; exit 1; }
done

echo "== Analyse à l'arrivée (file des tâches)"
"$repo/cloud/service/target/release/bike360-worker" --bucket "$bucket" --table "$table" --queue "$queue" \
    --tool "$repo/target/release/bike360-tool" --work "$work/worker" --once > "$work/worker.log" 2>&1 \
    || { echo "   ✗ exécutant en échec (voir $work/worker.log)" >&2; exit 1; }
grep -q "analyse de VID_" "$work/worker.log" && echo "   ✓ $(grep 'analyse de VID_' "$work/worker.log" | head -1 | sed -E 's/_[A-Z0-9]{4} :/_<caméra> :/')" \
    || { echo "   ✗ aucune tâche traitée (voir $work/worker.log)" >&2; exit 1; }
waiting=$(aws sqs get-queue-attributes --queue-url "$queue" --attribute-names ApproximateNumberOfMessages --query 'Attributes.ApproximateNumberOfMessages' --output text)
[ "$waiting" = 0 ] && echo "   ✓ file vide après traitement" || { echo "   ✗ $waiting tâche(s) restée(s) dans la file" >&2; exit 1; }

echo "== Bibliothèque"
node "$repo/cloud/essai-bibliotheque.mjs" "http://127.0.0.1:$port" "$lrv"
left=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "originaux/$client/" --query 'length(Contents || `[]`)' --output text)
left2=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "apercus/$client/" --query 'length(Contents || `[]`)' --output text)
left3=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "donnees/$client/" --query 'length(Contents || `[]`)' --output text)
[ "$left" = 0 ] && [ "$left2" = 0 ] && [ "$left3" = 0 ] && echo "   ✓ plus aucun fichier du client dans le stockage" \
    || { echo "   ✗ fichiers restants : $left originaux, $left2 aperçus, $left3 résultats d'analyse" >&2; exit 1; }
rows=$(aws dynamodb query --table-name "$table" --key-condition-expression 'pk = :c' \
    --expression-attribute-values "{\":c\":{\"S\":\"client#$client\"}}" --query Count --output text)
[ "$rows" = 0 ] && echo "   ✓ plus aucune ligne du client dans l'index" || { echo "   ✗ $rows ligne(s) restée(s) dans l'index" >&2; exit 1; }
echo "Essai réussi."
