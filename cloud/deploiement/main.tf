# Déploiement de Bike360 Cloud sur un compte AWS, région de Stockholm, sans serveur à entretenir :
#   socle     stockage des rushs, index, file, comptes (module ../terraform/socle, le même que l'essai local)
#   site.tf   site vitrine et interface dans S3, servis par CloudFront, qui relaie /api/* au service
#   service.tf  le service (comptes, envoi, bibliothèque, paliers, paiement) en fonction Lambda derrière
#               API Gateway, et le passage des échéances déclenché toutes les heures
#   worker.tf   l'analyse à l'arrivée, en tâche Fargate lancée quand la file se remplit
#
# Un environnement = un fichier env/<nom>.tfvars et un état séparé. Tout passe par cloud/deployer.sh :
#   sh cloud/deployer.sh essai            sh cloud/deployer.sh production
# Les secrets (clé Mollie) ne sont ni ici ni dans l'état : cloud/secrets.sh les range dans Parameter Store.
#
# L'atelier de montage n'est pas déployé ici : il tourne encore en processus sur la machine du
# service (voir docs/deploiement-aws.md, « Ce qui n'est pas encore déployé »).

terraform {
  required_version = ">= 1.10"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
  }
  # compartiment et clé donnés par cloud/deployer.sh (un état par environnement)
  backend "s3" {
    region       = "eu-north-1"
    encrypt      = true
    use_lockfile = true
  }
}

variable "env" {
  description = "Nom de l'environnement : « essai » ou « production »."
  type        = string
  validation {
    condition     = can(regex("^[a-z][a-z0-9]{1,15}$", var.env))
    error_message = "Lettres minuscules et chiffres seulement."
  }
}

variable "compte" {
  description = "Numéro du compte AWS : il rend uniques les noms des compartiments. Donné par cloud/deployer.sh."
  type        = string
  validation {
    condition     = can(regex("^[0-9]{12}$", var.compte))
    error_message = "Douze chiffres attendus."
  }
}

variable "region" {
  type    = string
  default = "eu-north-1" # Stockholm : la région du projet AWS et du chiffrage
}

variable "domain" {
  description = "Nom de domaine du site (« bike360.exemple »). Vide : le site répond à l'adresse que CloudFront lui donne."
  type        = string
  default     = ""
}

variable "zone_id" {
  description = "Zone Route 53 du domaine, si le domaine y est géré : les enregistrements sont alors créés ici. Vide : ils sont donnés en sortie, à créer chez le registraire."
  type        = string
  default     = ""
}

variable "certificate_ready" {
  description = "Hors Route 53 : passer à true une fois les enregistrements de validation du certificat créés chez le registraire, pour rattacher le domaine au site."
  type        = bool
  default     = false
}

variable "image_tag" {
  description = "Étiquette de l'image du service dans le dépôt d'images. Donnée par cloud/deployer.sh."
  type        = string
}

variable "mail_from" {
  description = "Expéditeur des courriels du service (« Bike360 <bonjour@bike360.exemple> »). Vide : pas de courriel hors ceux des comptes."
  type        = string
  default     = ""
}

variable "seller" {
  description = "Identité du vendeur portée sur les reçus : nom, adresse, SIREN, séparés par des virgules."
  type        = string
  default     = ""
}

variable "worker" {
  description = "Déploie l'analyse à l'arrivée (tâche Fargate lancée quand la file se remplit)."
  type        = bool
  default     = true
}

variable "alert_email" {
  description = "Adresse prévenue quand la dépense du mois approche du plafond. Vide : pas d'alerte."
  type        = string
  default     = ""
}

variable "budget_usd" {
  description = "Plafond de dépense mensuelle surveillé, en dollars."
  type        = number
  default     = 50
}

variable "protect" {
  description = "Empêche la suppression de la table et du groupe d'utilisateurs (à activer en production)."
  type        = bool
  default     = false
}

variable "offline" {
  description = "Vérification sans compte AWS (terraform plan à blanc) : aucun identifiant n'est demandé."
  type        = bool
  default     = false
}

provider "aws" {
  region                      = var.region
  access_key                  = var.offline ? "verification" : null
  secret_key                  = var.offline ? "verification" : null
  skip_credentials_validation = var.offline
  skip_metadata_api_check     = var.offline
  skip_requesting_account_id  = var.offline
  default_tags {
    tags = { projet = "bike360", environnement = var.env }
  }
}

# CloudFront ne lit ses certificats que dans la région de Virginie du Nord.
provider "aws" {
  alias                       = "virginie"
  region                      = "us-east-1"
  access_key                  = var.offline ? "verification" : null
  secret_key                  = var.offline ? "verification" : null
  skip_credentials_validation = var.offline
  skip_metadata_api_check     = var.offline
  skip_requesting_account_id  = var.offline
  default_tags {
    tags = { projet = "bike360", environnement = var.env }
  }
}

locals {
  name = "bike360-${var.env}"
  # le domaine n'est rattaché au site qu'une fois son certificat validé
  attached = var.domain != "" && (var.zone_id != "" || var.certificate_ready)
  site     = local.attached ? "https://${var.domain}" : "https://${aws_cloudfront_distribution.site.domain_name}"
  # chemin de Parameter Store où cloud/secrets.sh range les secrets de cet environnement
  secrets = "/bike360/${var.env}/"
}

module "socle" {
  source  = "../terraform/socle"
  prefix  = local.name
  bucket  = "${local.name}-rushs-${var.compte}"
  site    = local.site
  protect = var.protect
}

# ---------------------------------------------------------------- garde-fou de dépense

resource "aws_budgets_budget" "mois" {
  count        = var.alert_email != "" ? 1 : 0
  name         = "${local.name}-mois"
  budget_type  = "COST"
  limit_amount = tostring(var.budget_usd)
  limit_unit   = "USD"
  time_unit    = "MONTHLY"
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.alert_email]
  }
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.alert_email]
  }
}

# ---------------------------------------------------------------- sorties

output "site" {
  description = "Adresse publique du site."
  value       = local.site
}

output "webhook_mollie" {
  description = "Adresse que le service donne à Mollie pour ses notifications (rien à régler chez Mollie)."
  value       = "${local.site}/api/paiement/mollie"
}

output "site_bucket" {
  value = aws_s3_bucket.site.id
}

output "distribution" {
  value = aws_cloudfront_distribution.site.id
}

output "image_repository" {
  value = aws_ecr_repository.service.repository_url
}

output "secrets_path" {
  value = local.secrets
}

output "dns" {
  description = "Enregistrements à créer chez le registraire quand le domaine n'est pas dans Route 53."
  value = var.domain == "" || var.zone_id != "" ? [] : concat(
    [for o in aws_acm_certificate.site[0].domain_validation_options : { nom = o.resource_record_name, type = o.resource_record_type, valeur = o.resource_record_value, role = "validation du certificat" }],
    [{ nom = var.domain, type = "CNAME ou ALIAS", valeur = aws_cloudfront_distribution.site.domain_name, role = "site" }],
    [for t in aws_sesv2_email_identity.domaine[0].dkim_signing_attributes[0].tokens : { nom = "${t}._domainkey.${var.domain}", type = "CNAME", valeur = "${t}.dkim.amazonses.com", role = "signature des courriels" }],
  )
}
