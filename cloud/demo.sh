#!/bin/sh
# Essai à la main de tout le service hébergé, sur cette machine, dans votre navigateur.
#   sh cloud/demo.sh                    lance tout et reste au premier plan (Ctrl-C pour arrêter)
# Rien ne sort de la machine : AWS est émulé (floci) et le prestataire de paiement est un faux
# serveur Mollie (cloud/faux-mollie.mjs), dont la page de paiement propose de payer, d'échouer ou d'annuler.
# Les comptes créés sont confirmés d'office, puisqu'aucun courriel n'est envoyé.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
work="$repo/cloud/demo"
port=8370
site="http://127.0.0.1:$port"
export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_DEFAULT_REGION=eu-north-1
export AWS_ENDPOINT_URL=http://127.0.0.1:4566 AWS_PAGER=""
mollie_port=12112
export BIKE360_MOLLIE_KEY=test_demo BIKE360_MOLLIE_API=http://127.0.0.1:$mollie_port
export BIKE360_VENDEUR="Vendeur de démonstration, 1 rue de l'Essai, 75000 Paris, SIREN 000 000 000"
# l'émulateur repart de zéro à chaque lancement : l'état Terraform de l'essai aussi
state="$work/terraform.tfstate"
tf() { (cd "$repo/cloud/terraform" && terraform "$@"); }
out() { tf output -state="$state" -raw "$1"; }

command -v node >/dev/null || { echo "Node est nécessaire au faux serveur de paiement" >&2; exit 1; }
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
    --ui "$repo/ui" --vitrine "$repo/site" --port "$port" > "$work/service.log" 2>&1 &
service=$!
node "$repo/cloud/faux-mollie.mjs" "$mollie_port" > "$work/faux-mollie.log" 2>&1 &
mollie=$!
"$repo/cloud/service/target/release/bike360-worker" --bucket "$bucket" --table "$table" --queue "$queue" \
    --tool "$repo/target/release/bike360-tool" --work "$work/worker" > "$work/worker.log" 2>&1 &
worker=$!
stop() {
    echo; echo "Arrêt (les ateliers ouverts sont enregistrés)…"
    kill "$service" "$worker" "$mollie" 2>/dev/null || true
    wait "$service" 2>/dev/null || true
    docker compose -f "$repo/cloud/compose.yml" down >/dev/null 2>&1
    exit 0
}
trap stop INT TERM
for _ in $(seq 1 30); do curl -sf -o /dev/null "$site/api/compte" && break; sleep 1; done
curl -sf -o /dev/null "$site/api/compte" || { echo "le service n'a pas démarré : voir $work/service.log" >&2; kill "$service" "$worker" "$mollie" 2>/dev/null; exit 1; }

cat <<TEXTE

Tout est lancé. Ouvrez dans votre navigateur :

    $site/                    le site vitrine
    $site/ui/compte.html      le compte

Parcours à essayer :
  1. Créer un compte (mot de passe de 10 caractères au moins). Aucun courriel ne part : le compte est
     confirmé d'office en quelques secondes ; cliquez « J'ai déjà un compte » et connectez-vous.
  2. Envoyer des rushs : choisir un dossier contenant des LRV_….lrv et VID_….insv.
  3. Mes rushs : vignette, distance et moments forts arrivent après l'analyse (quelques secondes).
  4. Ouvrir l'atelier, poser un clip, exporter ; l'export apparaît dans « Mes exports ».
  5. Mon palier : « Choisir » mène à la page de paiement du faux Mollie, où l'on paie, échoue ou
     annule. Au retour : palier, moyen de paiement, historique et reçu. Même chose pour le crédit
     d'export. Pour faire prélever une échéance par le faux Mollie (renouvellement, ou refus) :
         curl -s http://127.0.0.1:$mollie_port/_faux/etat | grep -o 'sub_[0-9a-f]*' | sort -u
         curl -s -X POST -d '{"status":"paid"}' http://127.0.0.1:$mollie_port/_faux/echeance/sub_…

Journaux : $work/service.log et $work/worker.log. Ctrl-C arrête tout ; rien n'est conservé.
TEXTE

# pas de courriel dans l'émulateur : les comptes en attente sont confirmés d'office
while kill -0 "$service" 2>/dev/null; do
    for user in $(aws cognito-idp list-users --user-pool-id "$pool" --query 'Users[?UserStatus==`UNCONFIRMED`].Username' --output text 2>/dev/null); do
        aws cognito-idp admin-confirm-sign-up --user-pool-id "$pool" --username "$user" >/dev/null 2>&1 && echo "compte confirmé : $user"
    done
    sleep 3
done
