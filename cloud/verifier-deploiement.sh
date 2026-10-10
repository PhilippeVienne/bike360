#!/bin/sh
# Vérifie le déploiement AWS sans compte AWS : mise en forme, validation, puis un plan à blanc de
# chaque environnement (rien n'est créé, aucun identifiant n'est demandé, l'état reste dans un
# dossier temporaire).
#   sh cloud/verifier-deploiement.sh
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
unset AWS_ENDPOINT_URL AWS_PROFILE
check() { echo "   ✓ $1"; }

terraform fmt -check -recursive "$repo/cloud/terraform" "$repo/cloud/deploiement" >/dev/null || { echo "   ✗ mise en forme : lancer terraform fmt -recursive cloud" >&2; exit 1; }
check "mise en forme de cloud/terraform et cloud/deploiement"
for script in deployer.sh secrets.sh verifier-deploiement.sh essai-envoi.sh demo.sh; do sh -n "$repo/cloud/$script"; done
check "syntaxe des scripts"

# copie de travail : mêmes fichiers, état local à la place du compartiment d'état
mkdir -p "$work/cloud"
cp -r "$repo/cloud/deploiement" "$work/cloud/deploiement"
mkdir -p "$work/cloud/terraform" && cp -r "$repo/cloud/terraform/socle" "$work/cloud/terraform/socle"
rm -rf "$work/cloud/deploiement/.terraform"
printf 'terraform {\n  backend "local" {}\n}\n' > "$work/cloud/deploiement/backend_override.tf"
tf() { terraform -chdir="$work/cloud/deploiement" "$@"; }
tf init -input=false >/dev/null
tf validate >/dev/null
check "terraform validate"

plan() {   # plan NOM VARIABLES… : nombre de ressources que le plan créerait
    name=$1; shift
    out=$(tf plan -input=false -lock=false -var offline=true -var compte=000000000000 -var image_tag=verification "$@" 2>&1) \
        || { echo "$out" | tail -30 >&2; echo "   ✗ plan « $name »" >&2; exit 1; }
    check "plan « $name » : $(echo "$out" | sed 's/\x1b\[[0-9;]*m//g' | grep '^Plan:')"
}
plan "essai, sans domaine" -var-file=env/essai.tfvars
plan "production, domaine dans Route 53" -var-file=env/production.tfvars -var domain=bike360.exemple -var zone_id=Z0000000000000 \
    -var "mail_from=Bike360 <bonjour@bike360.exemple>" -var alert_email=alerte@bike360.exemple
plan "production, domaine chez un autre registraire, certificat en attente" -var-file=env/production.tfvars -var domain=bike360.exemple
plan "production, domaine chez un autre registraire, certificat validé" -var-file=env/production.tfvars -var domain=bike360.exemple -var certificate_ready=true
plan "essai, sans l'analyse à l'arrivée" -var-file=env/essai.tfvars -var worker=false
echo "Déploiement vérifié (sans compte AWS : rien n'a été créé)."
