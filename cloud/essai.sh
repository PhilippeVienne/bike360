#!/bin/sh
# Essai local du socle Bike360 Cloud sur l'émulateur floci (aucun compte AWS, aucun coût).
#   sh cloud/essai.sh [APERÇU.lrv]
# Déroule ce que fera le service : création du stockage, envoi d'un aperçu par morceaux avec
# reprise, lecture partielle de sa télémétrie, file des calculs, index, puis lecture des rushs
# par le serveur en mode hébergé. S'arrête à la première vérification qui échoue.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
work="$repo/cloud/essai"
lrv=${1:-}
client=demo
port=8399
part_mb=8

export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_DEFAULT_REGION=eu-west-3
export AWS_ENDPOINT_URL=http://127.0.0.1:4566
export AWS_PAGER=""

step() { printf '\n== %s\n' "$1"; }
ok() { printf '   ✓ %s\n' "$1"; }
fail() { printf '   ✗ %s\n' "$1" >&2; exit 1; }

[ -n "$lrv" ] && [ -f "$lrv" ] || fail "donner un fichier .lrv : sh cloud/essai.sh /chemin/LRV_….lrv"
[ -x "$repo/target/release/bike360-server" ] || fail "serveur non compilé : cargo build --release"
name=$(basename "$lrv")
rm -rf "$work"
mkdir -p "$work/parts" "$work/rushs" "$work/data" "$work/cache" "$work/exports" "$work/root"

step "Émulateur et infrastructure"
docker compose -f "$repo/cloud/compose.yml" up -d >/dev/null 2>&1
for _ in $(seq 1 30); do aws s3api list-buckets >/dev/null 2>&1 && break; sleep 1; done
aws s3api list-buckets >/dev/null 2>&1 || fail "floci ne répond pas sur $AWS_ENDPOINT_URL"
(cd "$repo/cloud/terraform" && terraform init -input=false >/dev/null && terraform apply -auto-approve -input=false -var local=true >/dev/null)
bucket=$(cd "$repo/cloud/terraform" && terraform output -raw bucket)
queue=$(cd "$repo/cloud/terraform" && terraform output -raw file_gpu)
table=$(cd "$repo/cloud/terraform" && terraform output -raw table)
ok "compartiment $bucket, table $table, file $(basename "$queue")"
rules=$(aws s3api get-bucket-lifecycle-configuration --bucket "$bucket" --query 'length(Rules)' --output text)
[ "$rules" = 4 ] && ok "4 règles d'archivage en place" || fail "règles d'archivage : $rules au lieu de 4"

step "Envoi par morceaux avec reprise ($name, morceaux de $part_mb Mo)"
key="apercus/$client/$name"
split -b "${part_mb}M" -d -a 3 "$lrv" "$work/parts/p"
total=$(ls "$work/parts" | wc -l)
upload=$(aws s3api create-multipart-upload --bucket "$bucket" --key "$key" --storage-class INTELLIGENT_TIERING --query UploadId --output text)
send() {   # envoie le morceau numéro $1 (à partir de 1)
    aws s3api upload-part --bucket "$bucket" --key "$key" --upload-id "$upload" --part-number "$1" \
        --body "$work/parts/p$(printf '%03d' $(($1 - 1)))" >/dev/null
}
first=$(( (total + 1) / 2 ))
for n in $(seq 1 "$first"); do send "$n"; done
# coupure simulée : on redemande à S3 ce qu'il a déjà reçu, puis on n'envoie que le reste
have=$(aws s3api list-parts --bucket "$bucket" --key "$key" --upload-id "$upload" --query 'length(Parts)' --output text)
[ "$have" = "$first" ] && ok "après coupure, S3 annonce $have morceaux sur $total" || fail "reprise : $have morceaux annoncés, $first attendus"
for n in $(seq $((have + 1)) "$total"); do send "$n"; done
aws s3api list-parts --bucket "$bucket" --key "$key" --upload-id "$upload" \
    --query '{Parts: Parts[].{PartNumber: PartNumber, ETag: ETag}}' --output json > "$work/parts.json"
aws s3api complete-multipart-upload --bucket "$bucket" --key "$key" --upload-id "$upload" \
    --multipart-upload "file://$work/parts.json" >/dev/null
aws s3api get-object --bucket "$bucket" --key "$key" "$work/retour.lrv" >/dev/null
cmp -s "$lrv" "$work/retour.lrv" && ok "fichier assemblé identique à l'original ($(stat -c %s "$lrv") octets)" || fail "le fichier assemblé diffère"
class=$(aws s3api head-object --bucket "$bucket" --key "$key" --query StorageClass --output text)
ok "classe de stockage : $class"

step "Lecture partielle de la télémétrie"
aws s3api get-object --bucket "$bucket" --key "$key" --range bytes=-32 "$work/fin.bin" >/dev/null
[ "$(cat "$work/fin.bin")" = "8db42d694ccc418790edff439fe026bf" ] \
    && ok "les 32 derniers octets portent la signature Insta360 : la télémétrie se lit sans télécharger le fichier" \
    || fail "signature de fin de fichier absente"

step "Envoi depuis le navigateur (CORS)"
site=http://127.0.0.1:8360   # valeur par défaut de la variable Terraform « site »
allow=$(curl -s -o /dev/null -D - -X OPTIONS "$AWS_ENDPOINT_URL/$bucket/$key" -H "Origin: $site" \
    -H "Access-Control-Request-Method: PUT" | tr -d '\r' | grep -i '^access-control-allow-origin:' | cut -d' ' -f2 || true)
[ -n "$allow" ] && ok "origine autorisée : $allow" || printf '   ? %s\n' "pas d'en-tête CORS renvoyé par l'émulateur (à vérifier sur AWS)"

step "File des calculs GPU et index"
aws sqs send-message --queue-url "$queue" --message-body "{\"job\":\"horizon\",\"client\":\"$client\",\"lrv\":\"$key\"}" >/dev/null
aws sqs receive-message --queue-url "$queue" --query 'Messages[0].[Body,ReceiptHandle]' --output text > "$work/msg.txt"
grep -q '"job":"horizon"' "$work/msg.txt" && ok "tâche reçue par un lecteur de la file" || fail "tâche non reçue"
aws sqs delete-message --queue-url "$queue" --receipt-handle "$(cut -f2 "$work/msg.txt")"
aws dynamodb put-item --table-name "$table" --item \
    "{\"pk\":{\"S\":\"client#$client\"},\"sk\":{\"S\":\"rush#$name\"},\"octets\":{\"N\":\"$(stat -c %s "$lrv")\"},\"classe\":{\"S\":\"$class\"}}"
got=$(aws dynamodb query --table-name "$table" --key-condition-expression 'pk = :c and begins_with(sk, :r)' \
    --expression-attribute-values "{\":c\":{\"S\":\"client#$client\"},\":r\":{\"S\":\"rush#\"}}" --query Count --output text)
[ "$got" = 1 ] && ok "rush retrouvé dans l'index du client" || fail "index : $got rush trouvé"

step "Serveur en mode hébergé sur les rushs du client"
# floci n'émule pas le montage Amazon S3 Files : on recopie le dossier du client, ce que le montage rendrait inutile
aws s3 sync "s3://$bucket/apercus/$client/" "$work/rushs/" >/dev/null
HOME="$work" BIKE360_CLOUD=1 BIKE360_PASSWORD= BIKE360_ROOT="$work/root" BIKE360_DATA="$work/data" \
    BIKE360_CACHE="$work/cache" BIKE360_EXPORTS="$work/exports" \
    "$repo/target/release/bike360-server" "$work/rushs" --host 127.0.0.1 --port "$port" > "$work/serveur.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
for _ in $(seq 1 60); do curl -sf -o /dev/null "http://127.0.0.1:$port/api/sessions" && break; sleep 1; done
sessions=$(curl -s "http://127.0.0.1:$port/api/sessions")
echo "$sessions" | grep -q '"id"' && ok "session analysée : $(echo "$sessions" | grep -o '"duration": *[0-9]*' | head -1 | grep -o '[0-9]*$') s, caméra $(echo "$sessions" | grep -o '"model": *"[^"]*"' | head -1 | cut -d'"' -f4)" \
    || fail "aucune session analysée (voir $work/serveur.log)"
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/api/fs?path=/")
[ "$code" = 404 ] && ok "le disque du serveur n'est pas explorable" || fail "navigateur de dossiers accessible ($code)"

printf '\nEssai réussi. Non couverts par l'"'"'émulateur : montage Amazon S3 Files, facturation et délais des classes\nde stockage, CloudFront, GPU, droits IAM.\n'
