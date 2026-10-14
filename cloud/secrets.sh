#!/bin/sh
# Range les secrets d'un environnement dans AWS Systems Manager (Parameter Store), chiffrés, hors du
# dépôt et hors de l'état Terraform. Le service les lit lui-même au démarrage.
#   sh cloud/secrets.sh essai            demande chaque secret au clavier (rien ne s'affiche ni ne reste dans l'historique)
#   sh cloud/secrets.sh production voir  montre quels secrets sont rangés, sans leur valeur
#   sh cloud/secrets.sh essai google     range les identifiants OAuth de Google dans le groupe d'utilisateurs
# Secrets : MOLLIE_KEY, la clé d'API de Mollie (« test_… » pour l'essai, « live_… » pour la production).
set -eu
env=${1:-}
case "$env" in essai|production) ;; *) echo "usage : sh cloud/secrets.sh essai|production [voir|google]" >&2; exit 1 ;; esac
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-eu-north-1} AWS_PAGER=""
path="/bike360/$env"
aws sts get-caller-identity >/dev/null 2>&1 || { echo "pas d'identifiants AWS : voir docs/deploiement-aws.md, étape 2" >&2; exit 1; }

if [ "${2:-}" = voir ]; then
    aws ssm get-parameters-by-path --path "$path/" --query 'Parameters[].[Name,LastModifiedDate]' --output text
    exit 0
fi

if [ "${2:-}" = google ]; then
    # Connexion par Google : Cognito garde lui-même l'identifiant et le secret du client OAuth créé dans
    # Google Cloud Console. Adresse de retour à y autoriser : https://<domaine OAuth du groupe>/oauth2/idpresponse
    pool=$(aws cognito-idp list-user-pools --max-results 60 --query "UserPools[?Name=='bike360-$env'].Id | [0]" --output text)
    [ -n "$pool" ] && [ "$pool" != None ] || { echo "groupe d'utilisateurs bike360-$env introuvable : déployer d'abord  sh cloud/deployer.sh $env" >&2; exit 1; }
    printf 'Identifiant du client OAuth Google (…apps.googleusercontent.com) : '
    read -r id
    printf 'Secret de ce client (rien ne s'"'"'affiche) : '
    stty -echo; read -r secret; stty echo; echo
    [ -n "$id" ] && [ -n "$secret" ] || { echo "abandon : identifiant ou secret vide" >&2; exit 1; }
    case "$id$secret" in *\"*|*\\*) echo "refusé : guillemet ou barre oblique inverse dans une valeur" >&2; exit 1 ;; esac
    # les valeurs passent par un fichier JSON éphémère, jamais par la ligne de commande
    tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT; chmod 600 "$tmp"
    details=$(printf '"ProviderDetails":{"client_id":"%s","client_secret":"%s","authorize_scopes":"openid email"},"AttributeMapping":{"email":"email","username":"sub"}' "$id" "$secret")
    if aws cognito-idp describe-identity-provider --user-pool-id "$pool" --provider-name Google >/dev/null 2>&1; then
        printf '{"UserPoolId":"%s","ProviderName":"Google",%s}' "$pool" "$details" > "$tmp"
        aws cognito-idp update-identity-provider --cli-input-json "file://$tmp" >/dev/null
    else
        printf '{"UserPoolId":"%s","ProviderName":"Google","ProviderType":"Google",%s}' "$pool" "$details" > "$tmp"
        aws cognito-idp create-identity-provider --cli-input-json "file://$tmp" >/dev/null
    fi
    echo "   ✓ fournisseur Google rangé dans le groupe bike360-$env"
    echo "Reste à passer google_login = true dans cloud/deploiement/env/$env.tfvars, puis  sh cloud/deployer.sh $env"
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
