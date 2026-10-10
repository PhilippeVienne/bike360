# Socle de Bike360 Cloud : rushs et exports dans S3, index dans DynamoDB, file des calculs, comptes.
# Module commun à l'essai local (../main.tf, sur l'émulateur floci) et au déploiement sur AWS
# (../../deploiement). Les objets sont rangés par nature puis par client (apercus/<client>/…,
# originaux/<client>/…) : les règles d'archivage S3 se déclarent par préfixe, donc la nature vient en premier.

terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.0"
    }
  }
}

variable "prefix" {
  description = "Préfixe des noms de la table, des files et du groupe d'utilisateurs (un par environnement)."
  type        = string
  default     = "bike360"
}

variable "bucket" {
  description = "Nom du compartiment S3 (unique au monde sur AWS)."
  type        = string
}

variable "site" {
  description = "Origine de l'interface web, autorisée à envoyer des rushs depuis le navigateur."
  type        = string
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

variable "protect" {
  description = "Empêche la suppression de la table et du groupe d'utilisateurs, et garde de quoi restaurer la table 35 jours."
  type        = bool
  default     = false
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
  name         = var.prefix
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "pk"
  range_key    = "sk"

  deletion_protection_enabled = var.protect
  dynamic "point_in_time_recovery" {
    for_each = var.protect ? [1] : []
    content {
      enabled = true
    }
  }

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
  name                      = "${var.prefix}-gpu-rebut"
  message_retention_seconds = 1209600 # 14 jours pour comprendre un échec
}

# Un message = un job.json du moteur GPU (horizon, floutage, export).
resource "aws_sqs_queue" "gpu" {
  name                       = "${var.prefix}-gpu"
  visibility_timeout_seconds = 3600 # un export long ne doit pas être repris par une seconde machine
  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.gpu_rebut.arn
    maxReceiveCount     = 3
  })
}

# ---------------------------------------------------------------- comptes

# Un compte = une adresse de courriel confirmée. L'identifiant du compte sert de préfixe à ses rushs.
resource "aws_cognito_user_pool" "comptes" {
  name                     = var.prefix
  deletion_protection      = var.protect ? "ACTIVE" : "INACTIVE"
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
  name                          = "${var.prefix}-web"
  user_pool_id                  = aws_cognito_user_pool.comptes.id
  generate_secret               = false
  explicit_auth_flows           = ["ALLOW_USER_PASSWORD_AUTH", "ALLOW_REFRESH_TOKEN_AUTH"]
  prevent_user_existence_errors = "ENABLED"
}

output "pool" {
  value = aws_cognito_user_pool.comptes.id
}

output "pool_arn" {
  value = aws_cognito_user_pool.comptes.arn
}

output "pool_endpoint" {
  value = aws_cognito_user_pool.comptes.endpoint
}

output "app_client" {
  value = aws_cognito_user_pool_client.web.id
}

output "bucket" {
  value = aws_s3_bucket.rushs.id
}

output "bucket_arn" {
  value = aws_s3_bucket.rushs.arn
}

output "table" {
  value = aws_dynamodb_table.index.name
}

output "table_arn" {
  value = aws_dynamodb_table.index.arn
}

output "file_gpu" {
  value = aws_sqs_queue.gpu.url
}

output "file_gpu_arn" {
  value = aws_sqs_queue.gpu.arn
}

output "file_gpu_name" {
  value = aws_sqs_queue.gpu.name
}
