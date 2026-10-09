#!/bin/sh
# Essai local du service commun sur l'émulateur floci : comptes, paliers et paiement, Envoi, analyse à
# l'arrivée, atelier à la demande, Bibliothèque.
#   sh cloud/essai-envoi.sh APERÇU.lrv
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
lrv=${1:-}
port=8371
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
rm -rf "$work"; mkdir -p "$work"
# grille de l'essai : celle par défaut, sauf 9 secondes d'export au palier d'essai (un export passe, le suivant non)
cat > "$work/paliers.json" <<'JSON'
[{"key": "essai", "label": "Essai", "quota_go": 128, "export_min": 0.15, "eur_year": 0},
 {"key": "200go", "label": "200 Go", "quota_go": 200, "export_min": 15, "eur_year": 19},
 {"key": "600go", "label": "600 Go", "quota_go": 600, "export_min": 30, "eur_year": 39},
 {"key": "1to", "label": "1 To", "quota_go": 1000, "export_min": 60, "eur_year": 65},
 {"key": "2to", "label": "2 To", "quota_go": 2000, "export_min": 120, "eur_year": 105}]
JSON
# paiement : clés factices, appels dirigés vers l'émulateur du prestataire
export BIKE360_STRIPE_KEY=sk_test_essai BIKE360_STRIPE_WEBHOOK_SECRET=whsec_essai BIKE360_STRIPE_API=http://127.0.0.1:12111
export BIKE360_STRIPE_PRICES=200go=price_200,600go=price_600,1to=price_1to,2to=price_2to
# garde en corbeille nulle : l'essai vide la corbeille tout de suite
BIKE360_TRASH_DAYS=0 BIKE360_PART_MB=8 "$repo/cloud/service/target/release/bike360-envoi" --bucket "$bucket" --table "$table" --queue "$queue" \
    --issuer "$issuer" --app-client "$app_client" \
    --atelier-bin "$repo/target/release/bike360-server" --atelier-work "$work/ateliers" --plans "$work/paliers.json" \
    --ui "$repo/ui" --port "$port" > "$work/envoi.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
for _ in $(seq 1 30); do curl -sf -o /dev/null "http://127.0.0.1:$port/api/compte" && break; sleep 1; done
echo "   ✓ service d'envoi sur le port $port, compartiment $bucket"

echo "== Comptes"
node "$repo/cloud/essai-comptes.mjs" "http://127.0.0.1:$port" "$pool" "$work/jetons.json"
token() { node -e "console.log(JSON.parse(require('fs').readFileSync('$work/jetons.json'))['$1']['$2'])"; }
export BIKE360_TOKEN=$(token a token) BIKE360_TOKEN_B=$(token b token)
client=$(token a sub)

echo "== Paliers, quotas et paiement"
node "$repo/cloud/essai-paiement.mjs" "http://127.0.0.1:$port" "$work/jetons.json" "$BIKE360_STRIPE_WEBHOOK_SECRET"

echo "== Envoi par le code du navigateur"
node "$repo/cloud/essai-envoi.mjs" "http://127.0.0.1:$port" "$lrv"

echo "== Contrôle dans le stockage"
aws s3api get-object --bucket "$bucket" --key "apercus/$client/$name" "$repo/cloud/essai-retour.lrv" >/dev/null
cmp -s "$lrv" "$repo/cloud/essai-retour.lrv" && echo "   ✓ fichier assemblé identique à l'original" || { echo "   ✗ le fichier assemblé diffère" >&2; exit 1; }
rm -f "$repo/cloud/essai-retour.lrv"
item=$(aws dynamodb get-item --table-name "$table" --key "{\"pk\":{\"S\":\"client#$client\"},\"sk\":{\"S\":\"rush#$name\"}}" \
    --query 'Item.[camera_modele.S, duree_s.N, nature.S, session.S]' --output text | sed -E 's/_[A-Z0-9]{4}$/_<caméra>/')
[ -n "$item" ] && [ "$item" != None ] && echo "   ✓ index : $item" || { echo "   ✗ rush absent de l'index" >&2; exit 1; }
for page in compte envoi bibliotheque palier; do
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

echo "== Atelier à la demande"
# l'original du même segment, s'il est à côté de l'aperçu, sert à essayer l'export final
insv=$(echo "$lrv" | sed -E 's|LRV_([0-9]{8}_[0-9]{6})_[0-9]{2}_([0-9]{3})\.lrv$|VID_\1_00_\2.insv|')
[ -f "$insv" ] || { insv=""; echo "   ? original absent à côté de l'aperçu : export final non essayé"; }
# un segment qui n'est pas le premier de sa session commence après l'heure de son nom : l'écart sert à caler la trace d'essai
shot=$(ffprobe -v error -show_entries format_tags=creation_time -of csv=p=0 "$lrv" | cut -c12-19)
named=$(basename "$lrv" | sed -E 's/^LRV_[0-9]{8}_([0-9]{2})([0-9]{2})([0-9]{2})_.*/\1:\2:\3/')
export BIKE360_DECALAGE_S=$(( $(date -u -d "1970-01-01 $shot" +%s) + 7200 - $(date -u -d "1970-01-01 $named" +%s) ))
node "$repo/cloud/essai-atelier.mjs" "http://127.0.0.1:$port" "$work/jetons.json" "$lrv" ${insv:+"$insv"}
saved=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "donnees/$client/atelier/selections/" --query 'length(Contents || `[]`)' --output text)
[ "$saved" -ge 1 ] && echo "   ✓ le clip du compte est dans le stockage" || { echo "   ✗ travail de l'atelier absent du stockage" >&2; exit 1; }
aws s3api head-object --bucket "$bucket" --key "donnees/$client/gps/essai.gpx" >/dev/null 2>&1 \
    && echo "   ✓ la trace GPS de l'atelier est rangée avec celles que lit l'analyse" || { echo "   ✗ trace GPS absente de donnees/<client>/gps/" >&2; exit 1; }
# la trace a fait redemander l'analyse des sessions de ce jour : l'exécutant la refait
"$repo/cloud/service/target/release/bike360-worker" --bucket "$bucket" --table "$table" --queue "$queue" \
    --tool "$repo/target/release/bike360-tool" --work "$work/worker" --once >> "$work/worker.log" 2>&1
export BIKE360_GPS_ATTENDU=1

echo "== Bibliothèque"
node "$repo/cloud/essai-bibliotheque.mjs" "http://127.0.0.1:$port" "$lrv"
left=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "originaux/$client/" --query 'length(Contents || `[]`)' --output text)
left2=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "apercus/$client/" --query 'length(Contents || `[]`)' --output text)
left3=$(aws s3api list-objects-v2 --bucket "$bucket" --prefix "donnees/$client/" --query 'length(Contents[?contains(Key, `VID_`)] || `[]`)' --output text)
[ "$left" = 0 ] && [ "$left2" = 0 ] && [ "$left3" = 0 ] && echo "   ✓ plus aucun fichier de cette session dans le stockage" \
    || { echo "   ✗ fichiers restants : $left originaux, $left2 aperçus, $left3 résultats d'analyse" >&2; exit 1; }
# la consommation d'export du mois reste comptée : supprimer une session ne rend pas ses minutes
rows=0
for kind in rush session marque; do
    n=$(aws dynamodb query --table-name "$table" --key-condition-expression 'pk = :c and begins_with(sk, :k)' \
        --expression-attribute-values "{\":c\":{\"S\":\"client#$client\"},\":k\":{\"S\":\"$kind#\"}}" --query Count --output text)
    rows=$((rows + n))
done
[ "$rows" = 0 ] && echo "   ✓ plus aucune ligne de rush ou de session du client dans l'index" || { echo "   ✗ $rows ligne(s) restée(s) dans l'index" >&2; exit 1; }
echo "Essai réussi."
