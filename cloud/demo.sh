#!/bin/sh
# Essai à la main de tout le service hébergé, sur cette machine, dans votre navigateur.
#   sh cloud/demo.sh                    lance tout et reste au premier plan (Ctrl-C pour arrêter)
#   sh cloud/demo.sh payer EMAIL PALIER simule la confirmation de paiement d'un palier (600go, 1to…)
#   sh cloud/demo.sh crediter EMAIL MINUTES simule le paiement de minutes d'export
# Rien ne sort de la machine : AWS et le prestataire de paiement sont émulés (floci, stripe-mock).
# Les comptes créés sont confirmés d'office, puisqu'aucun courriel n'est envoyé.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
work="$repo/cloud/demo"
port=8370
site="http://127.0.0.1:$port"
export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_DEFAULT_REGION=eu-west-3
export AWS_ENDPOINT_URL=http://127.0.0.1:4566 AWS_PAGER=""
export BIKE360_STRIPE_KEY=sk_test_demo BIKE360_STRIPE_WEBHOOK_SECRET=whsec_demo BIKE360_STRIPE_API=http://127.0.0.1:12111
export BIKE360_STRIPE_PRICES=200go=price_200,600go=price_600,1to=price_1to,2to=price_2to
# l'émulateur repart de zéro à chaque lancement : l'état Terraform de l'essai aussi
state="$work/terraform.tfstate"
tf() { (cd "$repo/cloud/terraform" && terraform "$@"); }
out() { tf output -state="$state" -raw "$1"; }

if [ "${1:-}" = payer ] || [ "${1:-}" = crediter ]; then
    [ $# = 3 ] || { echo "usage : sh cloud/demo.sh payer EMAIL PALIER | crediter EMAIL MINUTES" >&2; exit 1; }
    pool=$(out pool)
    sub=$(aws cognito-idp admin-get-user --user-pool-id "$pool" --username "$2" --query 'UserAttributes[?Name==`sub`].Value' --output text)
    [ -n "$sub" ] && [ "$sub" != None ] || { echo "compte inconnu : $2" >&2; exit 1; }
    if [ "$1" = payer ]; then
        what="\"metadata\":{\"palier\":\"$3\"},\"customer\":\"cus_demo\",\"subscription\":\"sub_demo\""
    else
        what="\"id\":\"cs_demo_$(date +%s)\",\"metadata\":{\"credit_min\":\"$3\"}"
    fi
    body="{\"type\":\"checkout.session.completed\",\"data\":{\"object\":{\"client_reference_id\":\"$sub\",\"payment_status\":\"paid\",$what}}}"
    t=$(date +%s)
    sig=$(printf '%s.%s' "$t" "$body" | openssl dgst -sha256 -hmac "$BIKE360_STRIPE_WEBHOOK_SECRET" | sed 's/^.* //')
    curl -s -X POST -H "Stripe-Signature: t=$t,v1=$sig" -d "$body" "$site/api/paiement/stripe"; echo
    exit 0
fi

for bin in "$repo/target/release/bike360-server" "$repo/target/release/bike360-tool" "$repo/cloud/service/target/release/bike360-envoi" "$repo/cloud/service/target/release/bike360-worker"; do
    [ -x "$bin" ] || { echo "à compiler d'abord : cargo build --release && cargo build --release --manifest-path cloud/service/Cargo.toml" >&2; exit 1; }
done
mkdir -p "$work"

echo "Démarrage des émulateurs et de l'infrastructure (une à deux minutes)…"
docker compose -f "$repo/cloud/compose.yml" down >/dev/null 2>&1 || true
rm -rf "$work"; mkdir -p "$work"
docker compose -f "$repo/cloud/compose.yml" up -d >/dev/null 2>&1
for _ in $(seq 1 30); do aws s3api list-buckets >/dev/null 2>&1 && break; sleep 1; done
# l'origine de la page doit être autorisée à déposer des morceaux dans le compartiment
tf init -input=false >/dev/null
tf apply -state="$state" -auto-approve -input=false -var local=true -var "site=$site" >/dev/null
bucket=$(out bucket); table=$(out table); queue=$(out file_gpu); pool=$(out pool)

"$repo/cloud/service/target/release/bike360-envoi" --bucket "$bucket" --table "$table" --queue "$queue" \
    --issuer "$(out issuer)" --app-client "$(out app_client)" \
    --atelier-bin "$repo/target/release/bike360-server" --atelier-work "$work/ateliers" \
    --ui "$repo/ui" --port "$port" > "$work/service.log" 2>&1 &
service=$!
"$repo/cloud/service/target/release/bike360-worker" --bucket "$bucket" --table "$table" --queue "$queue" \
    --tool "$repo/target/release/bike360-tool" --work "$work/worker" > "$work/worker.log" 2>&1 &
worker=$!
stop() {
    echo; echo "Arrêt (les ateliers ouverts sont enregistrés)…"
    kill "$service" "$worker" 2>/dev/null || true
    wait "$service" 2>/dev/null || true
    docker compose -f "$repo/cloud/compose.yml" down >/dev/null 2>&1
    exit 0
}
trap stop INT TERM
for _ in $(seq 1 30); do curl -sf -o /dev/null "$site/api/compte" && break; sleep 1; done
curl -sf -o /dev/null "$site/api/compte" || { echo "le service n'a pas démarré : voir $work/service.log" >&2; kill "$service" "$worker" 2>/dev/null; exit 1; }

cat <<TEXTE

Tout est lancé. Ouvrez dans votre navigateur :

    $site/ui/compte.html

Parcours à essayer :
  1. Créer un compte (mot de passe de 10 caractères au moins). Aucun courriel ne part : le compte est
     confirmé d'office en quelques secondes ; cliquez « J'ai déjà un compte » et connectez-vous.
  2. Envoyer des rushs : choisir un dossier contenant des LRV_….lrv et VID_….insv.
  3. Mes rushs : vignette, distance et moments forts arrivent après l'analyse (quelques secondes).
  4. Ouvrir l'atelier, poser un clip, exporter ; l'export apparaît dans « Mes exports ».
  5. Changer de palier : le bouton mène à une page factice. Pour simuler le paiement :
         sh cloud/demo.sh payer VOTRE_EMAIL 600go
     Même chose pour le crédit d'export (page « Palier et crédit d'export ») :
         sh cloud/demo.sh crediter VOTRE_EMAIL 60

Journaux : $work/service.log et $work/worker.log. Ctrl-C arrête tout ; rien n'est conservé.
TEXTE

# pas de courriel dans l'émulateur : les comptes en attente sont confirmés d'office
while kill -0 "$service" 2>/dev/null; do
    for user in $(aws cognito-idp list-users --user-pool-id "$pool" --query 'Users[?UserStatus==`UNCONFIRMED`].Username' --output text 2>/dev/null); do
        aws cognito-idp admin-confirm-sign-up --user-pool-id "$pool" --username "$user" >/dev/null 2>&1 && echo "compte confirmé : $user"
    done
    sleep 3
done
