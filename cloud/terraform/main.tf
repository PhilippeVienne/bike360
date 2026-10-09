# Socle de stockage de Bike360 Cloud : rushs et exports dans S3, index dans DynamoDB, file des calculs GPU.
# Les objets sont rangés par nature puis par client (apercus/<client>/…, originaux/<client>/…) :
# les règles d'archivage S3 se déclarent par préfixe, donc la nature vient en premier.
#
#   Essai local (émulateur floci) : terraform apply -var local=true
#   Compte AWS                    : terraform apply -var bucket=<nom unique>

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
  default = "eu-west-3" # Paris : c'est la région du chiffrage
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

# ---------------------------------------------------------------- stockage

resource "aws_s3_bucket" "rushs" {
  bucket = var.bucket
}

resource "aws_s3_bucket_public_access_block" "rushs" {
  bucket                  = aws_s3_bucket.rushs.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# Le navigateur envoie les rushs directement, par morceaux : il lui faut l'en-tête ETag de chaque morceau.
resource "aws_s3_bucket_cors_configuration" "rushs" {
  bucket = aws_s3_bucket.rushs.id
  cors_rule {
    allowed_origins = [var.site]
    allowed_methods = ["PUT", "GET", "HEAD"]
    allowed_headers = ["*"]
    expose_headers  = ["ETag"]
    max_age_seconds = 3600
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "rushs" {
  bucket = aws_s3_bucket.rushs.id

  # Les originaux arrivent en Glacier Instant (classe choisie à l'envoi), puis partent en archive profonde.
  rule {
    id     = "originaux-vers-archive-profonde"
    status = "Enabled"
    filter {
      prefix = "originaux/"
    }
    transition {
      days          = var.archive_days
      storage_class = "DEEP_ARCHIVE"
    }
  }

  rule {
    id     = "exports-supprimes"
    status = "Enabled"
    filter {
      prefix = "exports/"
    }
    expiration {
      days = var.export_days
    }
  }

  # Rushs d'un abonnement terminé : le service les étiquette à la fin du délai d'accès, cette règle
  # les fait passer en archive profonde (180 jours facturés au minimum, soit la durée de leur garde).
  rule {
    id     = "rushs-archives"
    status = "Enabled"
    filter {
      tag {
        key   = "etat"
        value = "archive"
      }
    }
    transition {
      days          = 0
      storage_class = "DEEP_ARCHIVE"
    }
  }

  # Un envoi abandonné reste facturé tant que ses morceaux existent.
  rule {
    id     = "envois-interrompus"
    status = "Enabled"
    filter {
      prefix = ""
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}

# ---------------------------------------------------------------- index et file

# Table unique : pk = client, sk = nature et identifiant (balade, session, envoi en cours…).
resource "aws_dynamodb_table" "index" {
  name         = "bike360"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "pk"
  range_key    = "sk"

  attribute {
    name = "pk"
    type = "S"
  }
  attribute {
    name = "sk"
    type = "S"
  }
}

resource "aws_sqs_queue" "gpu_rebut" {
  name                      = "bike360-gpu-rebut"
  message_retention_seconds = 1209600 # 14 jours pour comprendre un échec
}

# Un message = un job.json du moteur GPU (horizon, floutage, export).
resource "aws_sqs_queue" "gpu" {
  name                       = "bike360-gpu"
  visibility_timeout_seconds = 3600 # un export long ne doit pas être repris par une seconde machine
  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.gpu_rebut.arn
    maxReceiveCount     = 3
  })
}

# ---------------------------------------------------------------- comptes

# Un compte = une adresse de courriel confirmée. L'identifiant du compte sert de préfixe à ses rushs.
resource "aws_cognito_user_pool" "comptes" {
  name                     = "bike360"
  username_attributes      = ["email"]
  auto_verified_attributes = ["email"]

  password_policy {
    minimum_length    = 10
    require_lowercase = true
    require_numbers   = true
    require_symbols   = false
    require_uppercase = false
  }

  account_recovery_setting {
    recovery_mechanism {
      name     = "verified_email"
      priority = 1
    }
  }
}

# Application sans secret : c'est le service, pas le navigateur, qui parle à Cognito.
resource "aws_cognito_user_pool_client" "web" {
  name                          = "bike360-web"
  user_pool_id                  = aws_cognito_user_pool.comptes.id
  generate_secret               = false
  explicit_auth_flows           = ["ALLOW_USER_PASSWORD_AUTH", "ALLOW_REFRESH_TOKEN_AUTH"]
  prevent_user_existence_errors = "ENABLED"
}

output "pool" {
  value = aws_cognito_user_pool.comptes.id
}

output "app_client" {
  value = aws_cognito_user_pool_client.web.id
}

# Émetteur des jetons : le service y lit les clés publiques qui les signent.
output "issuer" {
  value = var.local ? "${replace(var.endpoint, "127.0.0.1", "localhost")}/${aws_cognito_user_pool.comptes.id}" : "https://${aws_cognito_user_pool.comptes.endpoint}"
}

output "bucket" {
  value = aws_s3_bucket.rushs.id
}

output "table" {
  value = aws_dynamodb_table.index.name
}

output "file_gpu" {
  value = aws_sqs_queue.gpu.url
}
