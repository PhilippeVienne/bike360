# Site vitrine (site/) et interface (ui/) : fichiers statiques dans un compartiment privé, servis par
# CloudFront, qui relaie aussi /api/* au service. Une seule origine pour le navigateur : les témoins
# de session restent attachés au site.

resource "aws_s3_bucket" "site" {
  bucket = "${local.name}-site-${var.compte}"
}

resource "aws_s3_bucket_public_access_block" "site" {
  bucket                  = aws_s3_bucket.site.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_cloudfront_origin_access_control" "site" {
  name                              = "${local.name}-site"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# Seule cette distribution lit le compartiment.
resource "aws_s3_bucket_policy" "site" {
  bucket = aws_s3_bucket.site.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "cloudfront.amazonaws.com" }
      Action    = "s3:GetObject"
      Resource  = "${aws_s3_bucket.site.arn}/*"
      Condition = { StringEquals = { "AWS:SourceArn" = aws_cloudfront_distribution.site.arn } }
    }]
  })
}

# ---------------------------------------------------------------- certificat et domaine

resource "aws_acm_certificate" "site" {
  count             = var.domain != "" ? 1 : 0
  provider          = aws.virginie
  domain_name       = var.domain
  validation_method = "DNS"
  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_route53_record" "validation" {
  for_each = var.domain != "" && var.zone_id != "" ? {
    for o in aws_acm_certificate.site[0].domain_validation_options : o.domain_name => o
  } : {}
  zone_id         = var.zone_id
  name            = each.value.resource_record_name
  type            = each.value.resource_record_type
  records         = [each.value.resource_record_value]
  ttl             = 300
  allow_overwrite = true
}

# Attend que le certificat soit délivré (quelques minutes une fois les enregistrements en place).
resource "aws_acm_certificate_validation" "site" {
  count                   = local.attached ? 1 : 0
  provider                = aws.virginie
  certificate_arn         = aws_acm_certificate.site[0].arn
  validation_record_fqdns = var.zone_id != "" ? [for r in aws_route53_record.validation : r.fqdn] : null
}

resource "aws_route53_record" "site" {
  for_each = var.domain != "" && var.zone_id != "" ? toset(["A", "AAAA"]) : toset([])
  zone_id  = var.zone_id
  name     = var.domain
  type     = each.key
  alias {
    name                   = aws_cloudfront_distribution.site.domain_name
    zone_id                = aws_cloudfront_distribution.site.hosted_zone_id
    evaluate_target_health = false
  }
}

# ---------------------------------------------------------------- distribution

locals {
  # politiques gérées par AWS, désignées par leur identifiant (stable et public)
  cache_optimized      = "658327ea-f89d-4fab-a63d-7e88639e58f6" # CachingOptimized
  cache_disabled       = "4135ea2d-6df8-44a3-9df3-4b5a84be39ad" # CachingDisabled
  forward_all_but_host = "b689b0a8-53d0-40ab-baf2-68738e2966ac" # AllViewerExceptHostHeader
  security_headers     = "67f7725c-6f97-4210-82d7-5512b31e9d03" # SecurityHeadersPolicy
}

resource "aws_cloudfront_distribution" "site" {
  enabled             = true
  comment             = local.name
  default_root_object = "index.html"
  price_class         = "PriceClass_100" # Europe et Amérique du Nord : la clientèle est en France
  http_version        = "http2and3"
  is_ipv6_enabled     = true
  aliases             = local.attached ? [var.domain] : []

  origin {
    origin_id                = "site"
    domain_name              = aws_s3_bucket.site.bucket_regional_domain_name
    origin_access_control_id = aws_cloudfront_origin_access_control.site.id
  }

  origin {
    origin_id   = "service"
    domain_name = replace(aws_apigatewayv2_api.service.api_endpoint, "https://", "")
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    target_origin_id           = "site"
    viewer_protocol_policy     = "redirect-to-https"
    allowed_methods            = ["GET", "HEAD"]
    cached_methods             = ["GET", "HEAD"]
    compress                   = true
    cache_policy_id            = local.cache_optimized
    response_headers_policy_id = local.security_headers
  }

  # le service : jamais de cache, tout ce qu'envoie le navigateur est transmis (témoins, corps, requête)
  ordered_cache_behavior {
    path_pattern             = "/api/*"
    target_origin_id         = "service"
    viewer_protocol_policy   = "https-only"
    allowed_methods          = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods           = ["GET", "HEAD"]
    cache_policy_id          = local.cache_disabled
    origin_request_policy_id = local.forward_all_but_host
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    cloudfront_default_certificate = !local.attached
    acm_certificate_arn            = local.attached ? aws_acm_certificate_validation.site[0].certificate_arn : null
    ssl_support_method             = local.attached ? "sni-only" : null
    minimum_protocol_version       = local.attached ? "TLSv1.2_2021" : null
  }
}

# ---------------------------------------------------------------- courriels du service

# Domaine d'expédition des courriels (Amazon SES), signé par DKIM. Sur un compte neuf, SES n'écrit
# qu'aux adresses vérifiées tant que la sortie du « bac à sable » n'a pas été demandée au support.
resource "aws_sesv2_email_identity" "domaine" {
  count          = var.domain != "" ? 1 : 0
  email_identity = var.domain
}

resource "aws_route53_record" "dkim" {
  count   = var.domain != "" && var.zone_id != "" ? 3 : 0
  zone_id = var.zone_id
  name    = "${aws_sesv2_email_identity.domaine[0].dkim_signing_attributes[0].tokens[count.index]}._domainkey.${var.domain}"
  type    = "CNAME"
  ttl     = 1800
  records = ["${aws_sesv2_email_identity.domaine[0].dkim_signing_attributes[0].tokens[count.index]}.dkim.amazonses.com"]
}
