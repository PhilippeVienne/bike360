# Le service (bike360-envoi) : une fonction Lambda qui fait tourner le serveur HTTP tel quel, grâce à
# l'adaptateur web de Lambda embarqué dans l'image, derrière une API HTTP d'API Gateway. Rien ne
# tourne ni ne coûte quand personne ne s'en sert.

resource "aws_ecr_repository" "service" {
  name                 = local.name
  image_tag_mutability = "IMMUTABLE"
  force_delete         = !var.protect
  image_scanning_configuration {
    scan_on_push = true
  }
}

# Les images anciennes ne sont pas gardées : seul le stockage des dix dernières est payé.
resource "aws_ecr_lifecycle_policy" "service" {
  repository = aws_ecr_repository.service.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "dix dernières images"
      selection    = { tagStatus = "any", countType = "imageCountMoreThan", countNumber = 10 }
      action       = { type = "expire" }
    }]
  })
}

locals {
  image = "${aws_ecr_repository.service.repository_url}:${var.image_tag}"
  # réglages communs aux deux fonctions ; les secrets n'y sont pas, le service les lit lui-même sous local.secrets
  service_env = {
    BIKE360_BUCKET     = module.socle.bucket
    BIKE360_TABLE      = module.socle.table
    BIKE360_QUEUE      = module.socle.file_gpu
    BIKE360_ISSUER     = "https://${module.socle.pool_endpoint}"
    BIKE360_APP_CLIENT = module.socle.app_client
    BIKE360_SITE       = local.site
    BIKE360_SECRETS    = local.secrets
    BIKE360_PAYMENT    = "mollie"
    BIKE360_MAIL_FROM  = var.mail_from
    BIKE360_VENDEUR    = var.seller
    BIKE360_SWEEP_MIN  = "0" # pas de boucle dans une fonction : la règle planifiée ci-dessous fait le passage
    AWS_LWA_PORT       = "8370"
    # l'adaptateur attend que le service réponde avant de lui confier des requêtes
    AWS_LWA_READINESS_CHECK_PATH = "/api/compte"
  }
}

data "aws_iam_policy_document" "lambda" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "service" {
  name               = "${local.name}-service"
  assume_role_policy = data.aws_iam_policy_document.lambda.json
}

# Ce que le service a le droit de faire, et rien d'autre.
data "aws_iam_policy_document" "service" {
  statement {
    sid = "Rushs"
    actions = ["s3:GetObject", "s3:GetObjectVersion", "s3:PutObject", "s3:DeleteObject", "s3:DeleteObjectVersion", "s3:AbortMultipartUpload",
      "s3:ListMultipartUploadParts", "s3:RestoreObject", "s3:GetObjectTagging", "s3:PutObjectTagging", "s3:DeleteObjectTagging",
    "s3:GetObjectVersionTagging"]
    resources = ["${module.socle.bucket_arn}/*"]
  }
  statement {
    sid       = "ListeDesRushs"
    actions   = ["s3:ListBucket", "s3:ListBucketVersions", "s3:ListBucketMultipartUploads"]
    resources = [module.socle.bucket_arn]
  }
  statement {
    sid       = "Index"
    actions   = ["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:DeleteItem", "dynamodb:Query", "dynamodb:Scan"]
    resources = [module.socle.table_arn]
  }
  statement {
    sid       = "File"
    actions   = ["sqs:SendMessage"]
    resources = [module.socle.file_gpu_arn]
  }
  statement {
    sid       = "Comptes"
    actions   = ["cognito-idp:AdminGetUser"]
    resources = [module.socle.pool_arn]
  }
  statement {
    sid       = "Secrets"
    actions   = ["ssm:GetParametersByPath"]
    resources = ["arn:aws:ssm:${var.region}:${var.compte}:parameter${trimsuffix(local.secrets, "/")}"]
  }
  statement {
    sid       = "Courriels"
    actions   = ["ses:SendEmail"]
    resources = ["*"]
  }
  statement {
    sid       = "Journaux"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.service.arn}:*", "${aws_cloudwatch_log_group.echeances.arn}:*"]
  }
}

resource "aws_iam_role_policy" "service" {
  name   = "service"
  role   = aws_iam_role.service.id
  policy = data.aws_iam_policy_document.service.json
}

resource "aws_cloudwatch_log_group" "service" {
  name              = "/aws/lambda/${local.name}-service"
  retention_in_days = 90
}

resource "aws_cloudwatch_log_group" "echeances" {
  name              = "/aws/lambda/${local.name}-echeances"
  retention_in_days = 90
}

resource "aws_lambda_function" "service" {
  function_name = "${local.name}-service"
  role          = aws_iam_role.service.arn
  package_type  = "Image"
  image_uri     = local.image
  architectures = ["x86_64"]
  memory_size   = 512
  timeout       = 29 # API Gateway n'attend pas plus de 30 secondes
  image_config {
    entry_point = ["/usr/local/bin/bike360-envoi"]
  }
  environment {
    variables = local.service_env
  }
  depends_on = [aws_cloudwatch_log_group.service, aws_iam_role_policy.service]
}

# ---------------------------------------------------------------- API publique

resource "aws_apigatewayv2_api" "service" {
  name          = local.name
  protocol_type = "HTTP"
}

resource "aws_apigatewayv2_integration" "service" {
  api_id                 = aws_apigatewayv2_api.service.id
  integration_type       = "AWS_PROXY"
  integration_uri        = aws_lambda_function.service.invoke_arn
  payload_format_version = "2.0"
}

# Seules les routes du service sont publiées ; ses routes internes (/api/interne/…) ne le sont pas.
resource "aws_apigatewayv2_route" "service" {
  for_each  = toset(["/api/compte", "/api/compte/{proxy+}", "/api/envoi/{proxy+}", "/api/paiement/{proxy+}", "/api/bibliotheque", "/api/bibliotheque/{proxy+}", "/api/atelier", "/api/atelier/{proxy+}"])
  api_id    = aws_apigatewayv2_api.service.id
  route_key = "ANY ${each.key}"
  target    = "integrations/${aws_apigatewayv2_integration.service.id}"
}

resource "aws_apigatewayv2_stage" "service" {
  api_id      = aws_apigatewayv2_api.service.id
  name        = "$default"
  auto_deploy = true
  # garde-fou contre un emballement : au-delà, les requêtes sont refusées plutôt que facturées
  default_route_settings {
    throttling_burst_limit = 100
    throttling_rate_limit  = 50
  }
}

resource "aws_lambda_permission" "api" {
  statement_id  = "api"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.service.function_name
  principal     = "apigateway.amazonaws.com"
  source_arn    = "${aws_apigatewayv2_api.service.execution_arn}/*/*"
}

# ---------------------------------------------------------------- passage des échéances

# La même image, avec la route du passage ouverte : cette fonction n'a aucune adresse publique, seule
# la règle planifiée l'appelle. L'adaptateur transmet l'événement à la route donnée.
resource "aws_lambda_function" "echeances" {
  function_name = "${local.name}-echeances"
  role          = aws_iam_role.service.arn
  package_type  = "Image"
  image_uri     = local.image
  architectures = ["x86_64"]
  memory_size   = 512
  timeout       = 900
  image_config {
    entry_point = ["/usr/local/bin/bike360-envoi"]
  }
  environment {
    variables = merge(local.service_env, {
      BIKE360_SWEEP_ROUTE       = "true"
      AWS_LWA_PASS_THROUGH_PATH = "/api/interne/echeances"
    })
  }
  depends_on = [aws_cloudwatch_log_group.echeances, aws_iam_role_policy.service]
}

resource "aws_cloudwatch_event_rule" "echeances" {
  name                = "${local.name}-echeances"
  description         = "Passage sur les échéances des comptes (fin d'essai, archive, résiliation, renouvellement impayé)"
  schedule_expression = "rate(1 hour)"
}

resource "aws_cloudwatch_event_target" "echeances" {
  rule = aws_cloudwatch_event_rule.echeances.name
  arn  = aws_lambda_function.echeances.arn
}

resource "aws_lambda_permission" "echeances" {
  statement_id  = "planification"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.echeances.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.echeances.arn
}

# Prévient quand le service échoue (alerte par courriel, si une adresse est donnée).
resource "aws_sns_topic" "alertes" {
  count = var.alert_email != "" ? 1 : 0
  name  = "${local.name}-alertes"
}

resource "aws_sns_topic_subscription" "alertes" {
  count     = var.alert_email != "" ? 1 : 0
  topic_arn = aws_sns_topic.alertes[0].arn
  protocol  = "email"
  endpoint  = var.alert_email
}

resource "aws_cloudwatch_metric_alarm" "erreurs" {
  for_each            = var.alert_email != "" ? { service = aws_lambda_function.service.function_name, echeances = aws_lambda_function.echeances.function_name } : {}
  alarm_name          = "${local.name}-${each.key}-erreurs"
  alarm_description   = "La fonction ${each.value} échoue"
  namespace           = "AWS/Lambda"
  metric_name         = "Errors"
  dimensions          = { FunctionName = each.value }
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_sns_topic.alertes[0].arn]
}
