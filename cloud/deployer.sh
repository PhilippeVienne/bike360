#!/bin/sh
# Déploie Bike360 Cloud sur un compte AWS, en une commande.
#   sh cloud/deployer.sh essai               déploie l'environnement d'essai
#   sh cloud/deployer.sh production          déploie la production
#   sh cloud/deployer.sh essai plan          montre ce qui changerait, sans rien créer ni modifier
#   sh cloud/deployer.sh essai detruire      supprime l'environnement (refusé en production)
# Ce que fait un déploiement : état Terraform, image du service, infrastructure, puis site vitrine et
# interface. Il se relance sans risque : seul ce qui a changé est touché.
# Avant la première fois : docs/deploiement-aws.md.
set -eu
env=${1:-}
action=${2:-deployer}
case "$env" in essai|production) ;; *) echo "usage : sh cloud/deployer.sh essai|production [plan|detruire]" >&2; exit 1 ;; esac
repo=$(cd "$(dirname "$0")/.." && pwd)
tf="$repo/cloud/deploiement"
export AWS_DEFAULT_REGION=eu-north-1 AWS_PAGER=""
unset AWS_ENDPOINT_URL   # jamais l'émulateur local ici

for tool in aws terraform docker git; do
    command -v "$tool" >/dev/null || { echo "outil manquant : $tool" >&2; exit 1; }
done
account=$(aws sts get-caller-identity --query Account --output text 2>/dev/null) \
    || { echo "pas d'identifiants AWS : voir docs/deploiement-aws.md, étape 2" >&2; exit 1; }
echo "== Compte AWS $account, région $AWS_DEFAULT_REGION, environnement « $env »"
if [ "$env" = production ] && [ "$action" = deployer ]; then
    printf 'Déployer en PRODUCTION sur le compte %s ? Taper « production » pour confirmer : ' "$account"
    read -r answer; [ "$answer" = production ] || { echo "abandon"; exit 1; }
fi

echo "== État Terraform"
state="bike360-etat-$account"
if ! aws s3api head-bucket --bucket "$state" 2>/dev/null; then
    [ "$action" = plan ] && { echo "le compartiment d'état $state n'existe pas encore : un premier déploiement le crée" >&2; exit 1; }
    aws s3api create-bucket --bucket "$state" --create-bucket-configuration LocationConstraint="$AWS_DEFAULT_REGION" >/dev/null
    aws s3api put-bucket-versioning --bucket "$state" --versioning-configuration Status=Enabled
    aws s3api put-public-access-block --bucket "$state" \
        --public-access-block-configuration BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
    echo "   ✓ compartiment d'état $state créé (versions gardées, accès public bloqué)"
fi
terraform -chdir="$tf" init -input=false -reconfigure -backend-config="bucket=$state" -backend-config="key=$env/terraform.tfstate" >/dev/null
# étiquette de l'image : la révision du code, et l'heure si l'arbre de travail a des changements non enregistrés
tag=$(git -C "$repo" rev-parse --short HEAD)
git -C "$repo" diff --quiet HEAD -- core server cloud/service Cargo.lock || tag="$tag-$(date +%Y%m%d%H%M%S)"
vars="-var-file=$tf/env/$env.tfvars -var compte=$account"

if [ "$action" = detruire ]; then
    [ "$env" = production ] && { echo "la production ne se supprime pas par ce script" >&2; exit 1; }
    bucket=$(terraform -chdir="$tf" output -raw site_bucket 2>/dev/null) && aws s3 rm "s3://$bucket" --recursive >/dev/null || true
    # shellcheck disable=SC2086
    terraform -chdir="$tf" destroy -input=false $vars -var image_tag=aucune
    echo "Reste à la main, s'ils ne doivent pas être gardés : les rushs (compartiment bike360-$env-rushs-$account, s'il n'était pas vide), les secrets (/bike360/$env/) et le compartiment d'état $state."
    exit 0
fi
if [ "$action" = plan ]; then
    # shellcheck disable=SC2086
    terraform -chdir="$tf" plan -input=false $vars -var "image_tag=$tag"
    exit 0
fi

echo "== Secrets"
aws ssm get-parameter --name "/bike360/$env/MOLLIE_KEY" >/dev/null 2>&1 \
    || { echo "la clé Mollie n'est pas rangée : lancer d'abord  sh cloud/secrets.sh $env" >&2; exit 1; }
echo "   ✓ clé Mollie rangée sous /bike360/$env/"

echo "== Image du service ($tag)"
# shellcheck disable=SC2086
terraform -chdir="$tf" apply -input=false -auto-approve $vars -var "image_tag=$tag" -target=aws_ecr_repository.service >/dev/null
repository=$(terraform -chdir="$tf" output -raw image_repository)
if aws ecr describe-images --repository-name "bike360-$env" --image-ids "imageTag=$tag" >/dev/null 2>&1; then
    echo "   ✓ image déjà dans le dépôt"
else
    docker build --platform linux/amd64 --provenance=false -f "$tf/Dockerfile" -t "$repository:$tag" "$repo"
    aws ecr get-login-password | docker login --username AWS --password-stdin "${repository%%/*}" >/dev/null
    docker push "$repository:$tag" >/dev/null
    echo "   ✓ image poussée"
fi

echo "== Infrastructure"
# shellcheck disable=SC2086
terraform -chdir="$tf" apply -input=false $vars -var "image_tag=$tag"

echo "== Site vitrine et interface"
bucket=$(terraform -chdir="$tf" output -raw site_bucket)
aws s3 sync "$repo/site/" "s3://$bucket/" --delete --exclude "ui/*" >/dev/null
aws s3 sync "$repo/ui/" "s3://$bucket/ui/" --delete >/dev/null
aws cloudfront create-invalidation --distribution-id "$(terraform -chdir="$tf" output -raw distribution)" --paths '/*' >/dev/null
site=$(terraform -chdir="$tf" output -raw site)
echo "   ✓ fichiers déposés, cache de CloudFront vidé"

echo "== Contrôle"
for _ in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' "$site/api/compte" || true)
    [ "$code" = 200 ] && break; sleep 10
done
[ "$code" = 200 ] && echo "   ✓ le service répond : $site/api/compte" || echo "   ✗ le service ne répond pas encore ($code) : voir les journaux /aws/lambda/bike360-$env-service" >&2
code=$(curl -s -o /dev/null -w '%{http_code}' "$site/" || true)
[ "$code" = 200 ] && echo "   ✓ le site répond : $site/" || echo "   ✗ le site ne répond pas encore ($code) ; une distribution neuve met quelques minutes à se propager" >&2
terraform -chdir="$tf" output dns 2>/dev/null | grep -q nom && { echo "Enregistrements DNS à créer chez le registraire :"; terraform -chdir="$tf" output dns; }
echo "Déployé : $site"
