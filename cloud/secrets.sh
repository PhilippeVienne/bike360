#!/bin/sh
# Range les secrets d'un environnement dans AWS Systems Manager (Parameter Store), chiffrés, hors du
# dépôt et hors de l'état Terraform. Le service les lit lui-même au démarrage.
#   sh cloud/secrets.sh essai            demande chaque secret au clavier (rien ne s'affiche ni ne reste dans l'historique)
#   sh cloud/secrets.sh production voir  montre quels secrets sont rangés, sans leur valeur
# Secrets : MOLLIE_KEY, la clé d'API de Mollie (« test_… » pour l'essai, « live_… » pour la production).
set -eu
env=${1:-}
case "$env" in essai|production) ;; *) echo "usage : sh cloud/secrets.sh essai|production [voir]" >&2; exit 1 ;; esac
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-eu-north-1} AWS_PAGER=""
path="/bike360/$env"
aws sts get-caller-identity >/dev/null 2>&1 || { echo "pas d'identifiants AWS : voir docs/deploiement-aws.md, étape 2" >&2; exit 1; }

if [ "${2:-}" = voir ]; then
    aws ssm get-parameters-by-path --path "$path/" --query 'Parameters[].[Name,LastModifiedDate]' --output text
    exit 0
fi

printf 'Clé d'"'"'API Mollie pour « %s » (laisser vide pour ne pas la changer) : ' "$env"
stty -echo; read -r key; stty echo; echo
if [ -n "$key" ]; then
    case "$env:$key" in
        essai:test_*|production:live_*) ;;
        essai:live_*) echo "refusé : une clé « live_ » encaisse de vrais paiements, l'environnement d'essai prend une clé « test_ »" >&2; exit 1 ;;
        production:test_*) echo "refusé : la production prend une clé « live_ »" >&2; exit 1 ;;
        *) echo "refusé : une clé Mollie commence par « test_ » ou « live_ »" >&2; exit 1 ;;
    esac
    # la valeur passe par l'entrée standard d'un fichier JSON éphémère, jamais par la ligne de commande
    tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT; chmod 600 "$tmp"
    printf '{"Name":"%s/MOLLIE_KEY","Value":"%s","Type":"SecureString","Overwrite":true}' "$path" "$key" > "$tmp"
    aws ssm put-parameter --cli-input-json "file://$tmp" >/dev/null
    echo "   ✓ $path/MOLLIE_KEY rangée"
fi
echo "Le service lit ses secrets au démarrage : relancer sh cloud/deployer.sh $env pour qu'il prenne une clé changée."
