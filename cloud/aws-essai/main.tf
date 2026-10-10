# Essai réduit sur un vrai compte AWS : un compartiment monté avec Amazon S3 Files sur une machine
# GPU, pour lever ce que l'émulateur ne dit pas (classes de stockage lisibles par le montage,
# renommage, délai d'apparition d'un objet, débit) et mesurer le moteur GPU sur la carte d'AWS.
# Indépendant de cloud/terraform : il crée son propre compartiment et se supprime d'un bloc.
#
#   terraform init && terraform apply      puis suivre cloud/aws-essai/README.md
#   terraform destroy                      à la fin : la machine est facturée tant qu'elle existe

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.40" # ressources aws_s3files_*
    }
  }
}

variable "region" {
  type    = string
  default = "eu-north-1" # Stockholm : la région du projet AWS et du chiffrage
}

variable "instance_type" {
  description = "Machine GPU de l'essai. g4dn.xlarge porte la carte T4 retenue dans le chiffrage."
  type        = string
  default     = "g4dn.xlarge"
}

variable "spot" {
  description = "Machine au prix spot (environ 0,18 $/h au lieu de 0,62) ; elle peut être reprise par AWS."
  type        = bool
  default     = true
}

variable "subnet_id" {
  description = "Sous-réseau de la machine. Par défaut, le premier du réseau par défaut ; à préciser si la machine n'y est pas proposée."
  type        = string
  default     = ""
}

variable "repo" {
  type    = string
  default = "https://github.com/PhilippeVienne/bike360.git"
}

variable "branch" {
  type    = string
  default = "lot0-modele-donnees"
}

provider "aws" {
  region = var.region
  default_tags {
    tags = { projet = "bike360-essai" }
  }
}

data "aws_caller_identity" "me" {}

data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

# Image d'AWS avec pilote NVIDIA et kit CUDA (nvcc) : le moteur GPU s'y compile tel quel.
data "aws_ssm_parameter" "ami" {
  name = "/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-ubuntu-22.04/latest/ami-id"
}

locals {
  subnet = var.subnet_id != "" ? var.subnet_id : sort(data.aws_subnets.default.ids)[0]
  bucket = "bike360-essai-${data.aws_caller_identity.me.account_id}"
}

# ---------------------------------------------------------------- compartiment

resource "aws_s3_bucket" "essai" {
  bucket        = local.bucket
  force_destroy = true # l'essai fini, tout part avec terraform destroy
}

# S3 Files exige les versions d'objets pour synchroniser le montage et le compartiment.
resource "aws_s3_bucket_versioning" "essai" {
  bucket = aws_s3_bucket.essai.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "essai" {
  bucket                  = aws_s3_bucket.essai.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# Avec les versions, un objet remplacé ou supprimé reste facturé : on ne garde les anciennes qu'un jour.
resource "aws_s3_bucket_lifecycle_configuration" "essai" {
  bucket     = aws_s3_bucket.essai.id
  depends_on = [aws_s3_bucket_versioning.essai]
  rule {
    id     = "anciennes-versions"
    status = "Enabled"
    filter {
      prefix = ""
    }
    noncurrent_version_expiration {
      noncurrent_days = 1
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

# ---------------------------------------------------------------- montage S3 Files

# Rôle que S3 Files endosse pour lire et écrire le compartiment (modèle de la documentation d'AWS).
resource "aws_iam_role" "s3files" {
  name = "bike360-essai-s3files"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "AllowS3FilesAssumeRole"
      Effect    = "Allow"
      Principal = { Service = "elasticfilesystem.amazonaws.com" }
      Action    = "sts:AssumeRole"
      Condition = {
        StringEquals = { "aws:SourceAccount" = data.aws_caller_identity.me.account_id }
        ArnLike      = { "aws:SourceArn" = "arn:aws:s3files:${var.region}:${data.aws_caller_identity.me.account_id}:file-system/*" }
      }
    }]
  })
}

resource "aws_iam_role_policy" "s3files" {
  role = aws_iam_role.s3files.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "S3BucketPermissions"
        Effect    = "Allow"
        Action    = ["s3:ListBucket", "s3:ListBucketVersions"]
        Resource  = aws_s3_bucket.essai.arn
        Condition = { StringEquals = { "aws:ResourceAccount" = data.aws_caller_identity.me.account_id } }
      },
      {
        Sid       = "S3ObjectPermissions"
        Effect    = "Allow"
        Action    = ["s3:AbortMultipartUpload", "s3:DeleteObject*", "s3:GetObject*", "s3:List*", "s3:PutObject*"]
        Resource  = "${aws_s3_bucket.essai.arn}/*"
        Condition = { StringEquals = { "aws:ResourceAccount" = data.aws_caller_identity.me.account_id } }
      },
      {
        Sid       = "EventBridgeManage"
        Effect    = "Allow"
        Action    = ["events:DeleteRule", "events:DisableRule", "events:EnableRule", "events:PutRule", "events:PutTargets", "events:RemoveTargets"]
        Resource  = ["arn:aws:events:*:*:rule/DO-NOT-DELETE-S3-Files*"]
        Condition = { StringEquals = { "events:ManagedBy" = "elasticfilesystem.amazonaws.com" } }
      },
      {
        Sid      = "EventBridgeRead"
        Effect   = "Allow"
        Action   = ["events:DescribeRule", "events:ListRuleNamesByTarget", "events:ListRules", "events:ListTargetsByRule"]
        Resource = ["arn:aws:events:*:*:rule/*"]
      },
    ]
  })
}

resource "aws_s3files_file_system" "essai" {
  bucket     = aws_s3_bucket.essai.arn
  role_arn   = aws_iam_role.s3files.arn
  depends_on = [aws_s3_bucket_versioning.essai, aws_iam_role_policy.s3files]
}

resource "aws_security_group" "machine" {
  name        = "bike360-essai-machine"
  description = "Machine de l'essai : aucune entree (acces par Session Manager), sorties libres"
  vpc_id      = data.aws_vpc.default.id
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_security_group" "montage" {
  name        = "bike360-essai-montage"
  description = "Cible de montage S3 Files : NFS depuis la machine de l'essai seulement"
  vpc_id      = data.aws_vpc.default.id
  ingress {
    from_port       = 2049
    to_port         = 2049
    protocol        = "tcp"
    security_groups = [aws_security_group.machine.id]
  }
}

resource "aws_s3files_mount_target" "essai" {
  file_system_id  = aws_s3files_file_system.essai.id
  subnet_id       = local.subnet
  security_groups = [aws_security_group.montage.id]
}

# ---------------------------------------------------------------- machine GPU

resource "aws_iam_role" "machine" {
  name = "bike360-essai-machine"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

# Session Manager (terminal sans port ouvert ni clé SSH) et client S3 Files.
resource "aws_iam_role_policy_attachment" "machine" {
  for_each   = toset(["AmazonSSMManagedInstanceCore", "AmazonS3FilesClientFullAccess"])
  role       = aws_iam_role.machine.name
  policy_arn = "arn:aws:iam::aws:policy/${each.key}"
}

# Accès direct au compartiment : lectures rapides du montage, et dépôts d'objets pendant l'essai.
resource "aws_iam_role_policy" "machine" {
  role = aws_iam_role.machine.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = ["s3:GetObject", "s3:GetObjectVersion", "s3:PutObject", "s3:DeleteObject", "s3:RestoreObject"], Resource = "${aws_s3_bucket.essai.arn}/*" },
      { Effect = "Allow", Action = "s3:ListBucket", Resource = aws_s3_bucket.essai.arn },
    ]
  })
}

resource "aws_iam_instance_profile" "machine" {
  name = "bike360-essai-machine"
  role = aws_iam_role.machine.name
}

resource "aws_instance" "essai" {
  ami                         = data.aws_ssm_parameter.ami.value
  instance_type               = var.instance_type
  subnet_id                   = local.subnet
  associate_public_ip_address = true # pour télécharger le code et les outils ; aucune entrée n'est ouverte
  vpc_security_group_ids      = [aws_security_group.machine.id]
  iam_instance_profile        = aws_iam_instance_profile.machine.name
  depends_on                  = [aws_s3files_mount_target.essai]

  dynamic "instance_market_options" {
    for_each = var.spot ? [1] : []
    content {
      market_type = "spot"
    }
  }

  root_block_device {
    volume_size = 120 # compilation, exports et copie locale d'un original pour comparer les débits
    volume_type = "gp3"
  }

  metadata_options {
    http_tokens = "required"
  }

  user_data = templatefile("${path.module}/preparation.sh", {
    repo        = var.repo
    branch      = var.branch
    file_system = aws_s3files_file_system.essai.id
    bucket      = aws_s3_bucket.essai.id
    region      = var.region
  })

  tags = { Name = "bike360-essai" }
}

output "bucket" {
  value = aws_s3_bucket.essai.id
}

output "machine" {
  value = aws_instance.essai.id
}

output "terminal" {
  description = "Ouvre un terminal sur la machine (demande le module Session Manager de l'outil aws)."
  value       = "aws ssm start-session --region ${var.region} --target ${aws_instance.essai.id}"
}
