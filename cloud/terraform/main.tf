# Socle de Bike360 Cloud pour l'essai local : le module socle/ (stockage, index, file, comptes) appliqué
# à l'émulateur floci.
#
#   Essai local (émulateur floci) : terraform apply -var local=true
#   Compte AWS                    : voir ../deploiement et docs/deploiement-aws.md (sh cloud/deployer.sh)

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
  }
}

variable "local" {
  description = "Vrai pour viser l'émulateur floci au lieu d'AWS."
  type        = bool
  default     = false
}

variable "endpoint" {
  description = "Adresse de l'émulateur (essai local seulement)."
  type        = string
  default     = "http://127.0.0.1:4566"
}

variable "region" {
  type    = string
  default = "eu-north-1" # Stockholm : la région du projet AWS et du chiffrage
}

variable "bucket" {
  description = "Nom du compartiment S3 (unique au monde sur AWS)."
  type        = string
  default     = "bike360-rushs-local"
}

variable "site" {
  description = "Origine de l'interface web, autorisée à envoyer des rushs depuis le navigateur."
  type        = string
  default     = "http://127.0.0.1:8360"
}

variable "archive_days" {
  description = "Âge (jours) auquel un original passe en archive profonde. 90 est le minimum facturé en Glacier Instant."
  type        = number
  default     = 90
}

variable "export_days" {
  description = "Durée de conservation des exports (jours)."
  type        = number
  default     = 30
}

provider "aws" {
  region                      = var.region
  access_key                  = var.local ? "test" : null
  secret_key                  = var.local ? "test" : null
  skip_credentials_validation = var.local
  skip_metadata_api_check     = var.local
  skip_requesting_account_id  = var.local
  s3_use_path_style           = var.local

  dynamic "endpoints" {
    for_each = var.local ? [var.endpoint] : []
    content {
      s3         = endpoints.value
      dynamodb   = endpoints.value
      sqs        = endpoints.value
      cognitoidp = endpoints.value
    }
  }
}

module "socle" {
  source       = "./socle"
  bucket       = var.bucket
  site         = var.site
  archive_days = var.archive_days
  export_days  = var.export_days
}

# Les ressources étaient déclarées ici avant d'être rangées dans le module : un état existant suit.
moved {
  from = aws_s3_bucket.rushs
  to   = module.socle.aws_s3_bucket.rushs
}
moved {
  from = aws_s3_bucket_public_access_block.rushs
  to   = module.socle.aws_s3_bucket_public_access_block.rushs
}
moved {
  from = aws_s3_bucket_cors_configuration.rushs
  to   = module.socle.aws_s3_bucket_cors_configuration.rushs
}
moved {
  from = aws_s3_bucket_lifecycle_configuration.rushs
  to   = module.socle.aws_s3_bucket_lifecycle_configuration.rushs
}
moved {
  from = aws_dynamodb_table.index
  to   = module.socle.aws_dynamodb_table.index
}
moved {
  from = aws_sqs_queue.gpu_rebut
  to   = module.socle.aws_sqs_queue.gpu_rebut
}
moved {
  from = aws_sqs_queue.gpu
  to   = module.socle.aws_sqs_queue.gpu
}
moved {
  from = aws_cognito_user_pool.comptes
  to   = module.socle.aws_cognito_user_pool.comptes
}
moved {
  from = aws_cognito_user_pool_client.web
  to   = module.socle.aws_cognito_user_pool_client.web
}

output "pool" {
  value = module.socle.pool
}

output "app_client" {
  value = module.socle.app_client
}

# Émetteur des jetons : le service y lit les clés publiques qui les signent.
output "issuer" {
  value = var.local ? "${replace(var.endpoint, "127.0.0.1", "localhost")}/${module.socle.pool}" : "https://${module.socle.pool_endpoint}"
}

output "bucket" {
  value = module.socle.bucket
}

output "table" {
  value = module.socle.table
}

output "file_gpu" {
  value = module.socle.file_gpu
}
