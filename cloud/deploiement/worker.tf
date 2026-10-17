# Analyse à l'arrivée (bike360-worker) : une tâche Fargate lancée quand la file reçoit une tâche, et
# arrêtée quand elle est vide. Elle n'a besoin que d'un accès sortant : un petit réseau public, sans
# passerelle NAT (qui coûterait une trentaine d'euros par mois à elle seule).

locals {
  worker = var.worker ? 1 : 0
}

resource "aws_vpc" "worker" {
  count                = local.worker
  cidr_block           = "10.36.0.0/24"
  enable_dns_hostnames = true
  tags                 = { Name = local.name }
}

resource "aws_internet_gateway" "worker" {
  count  = local.worker
  vpc_id = aws_vpc.worker[0].id
}

resource "aws_subnet" "worker" {
  count                   = var.worker ? 2 : 0
  vpc_id                  = aws_vpc.worker[0].id
  cidr_block              = cidrsubnet("10.36.0.0/24", 1, count.index)
  availability_zone       = "${var.region}${["a", "b"][count.index]}"
  map_public_ip_on_launch = true
  tags                    = { Name = "${local.name}-${["a", "b"][count.index]}" }
}

resource "aws_route_table" "worker" {
  count  = local.worker
  vpc_id = aws_vpc.worker[0].id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.worker[0].id
  }
}

resource "aws_route_table_association" "worker" {
  count          = var.worker ? 2 : 0
  subnet_id      = aws_subnet.worker[count.index].id
  route_table_id = aws_route_table.worker[0].id
}

# Aucune entrée : la tâche appelle AWS, personne ne l'appelle.
resource "aws_security_group" "worker" {
  count       = local.worker
  name        = "${local.name}-worker"
  description = "Sortie seulement"
  vpc_id      = aws_vpc.worker[0].id
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

data "aws_iam_policy_document" "ecs" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

# Rôle avec lequel ECS tire l'image et écrit les journaux.
resource "aws_iam_role" "worker_start" {
  count              = local.worker
  name               = "${local.name}-worker-demarrage"
  assume_role_policy = data.aws_iam_policy_document.ecs.json
}

resource "aws_iam_role_policy_attachment" "worker_start" {
  count      = local.worker
  role       = aws_iam_role.worker_start[0].name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# Rôle de la tâche elle-même : lire les aperçus et les traces, déposer les résultats, tenir l'index, vider la file.
resource "aws_iam_role" "worker" {
  count              = local.worker
  name               = "${local.name}-worker"
  assume_role_policy = data.aws_iam_policy_document.ecs.json
}

data "aws_iam_policy_document" "worker" {
  statement {
    actions   = ["s3:GetObject"]
    resources = ["${module.socle.bucket_arn}/apercus/*", "${module.socle.bucket_arn}/donnees/*"]
  }
  statement {
    actions   = ["s3:PutObject"]
    resources = ["${module.socle.bucket_arn}/donnees/*"]
  }
  statement {
    actions   = ["s3:ListBucket"]
    resources = [module.socle.bucket_arn]
  }
  statement {
    actions   = ["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:Query"]
    resources = [module.socle.table_arn]
  }
  statement {
    actions   = ["sqs:ReceiveMessage", "sqs:DeleteMessage", "sqs:ChangeMessageVisibility", "sqs:GetQueueAttributes"]
    resources = [module.socle.file_gpu_arn]
  }
}

resource "aws_iam_role_policy" "worker" {
  count  = local.worker
  name   = "worker"
  role   = aws_iam_role.worker[0].id
  policy = data.aws_iam_policy_document.worker.json
}

resource "aws_cloudwatch_log_group" "worker" {
  count             = local.worker
  name              = "/ecs/${local.name}-worker"
  retention_in_days = 90
}

resource "aws_ecs_cluster" "worker" {
  count = local.worker
  name  = local.name
}

resource "aws_ecs_task_definition" "worker" {
  count                    = local.worker
  family                   = "${local.name}-worker"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 2048
  memory                   = 4096
  execution_role_arn       = aws_iam_role.worker_start[0].arn
  task_role_arn            = aws_iam_role.worker[0].arn
  # les aperçus d'une session sont copiés sur le disque de la tâche le temps de l'analyse
  ephemeral_storage {
    size_in_gib = 50
  }
  runtime_platform {
    cpu_architecture        = "X86_64"
    operating_system_family = "LINUX"
  }
  container_definitions = jsonencode([{
    name       = "worker"
    image      = local.image
    essential  = true
    entryPoint = ["/usr/local/bin/bike360-worker"]
    command    = ["--tool", "/usr/local/bin/bike360-tool"]
    environment = [
      { name = "BIKE360_BUCKET", value = module.socle.bucket },
      { name = "BIKE360_TABLE", value = module.socle.table },
      { name = "BIKE360_QUEUE", value = module.socle.file_gpu },
    ]
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.worker[0].name
        "awslogs-region"        = var.region
        "awslogs-stream-prefix" = "worker"
      }
    }
  }])
}

resource "aws_ecs_service" "worker" {
  count           = local.worker
  name            = "worker"
  cluster         = aws_ecs_cluster.worker[0].id
  task_definition = aws_ecs_task_definition.worker[0].arn
  desired_count   = 0 # la file décide : voir les alarmes ci-dessous
  capacity_provider_strategy {
    capacity_provider = "FARGATE_SPOT" # une tâche interrompue laisse son message dans la file, qui le représente
    weight            = 1
  }
  network_configuration {
    subnets          = aws_subnet.worker[*].id
    security_groups  = [aws_security_group.worker[0].id]
    assign_public_ip = true
  }
  lifecycle {
    ignore_changes = [desired_count]
  }
}

resource "aws_ecs_cluster_capacity_providers" "worker" {
  count              = local.worker
  cluster_name       = aws_ecs_cluster.worker[0].name
  capacity_providers = ["FARGATE", "FARGATE_SPOT"]
}

resource "aws_appautoscaling_target" "worker" {
  count              = local.worker
  service_namespace  = "ecs"
  resource_id        = "service/${aws_ecs_cluster.worker[0].name}/${aws_ecs_service.worker[0].name}"
  scalable_dimension = "ecs:service:DesiredCount"
  min_capacity       = 0
  max_capacity       = 1
}

resource "aws_appautoscaling_policy" "worker" {
  for_each           = var.worker ? { lancer = 1, arreter = 0 } : {}
  name               = "${local.name}-worker-${each.key}"
  service_namespace  = "ecs"
  resource_id        = aws_appautoscaling_target.worker[0].resource_id
  scalable_dimension = aws_appautoscaling_target.worker[0].scalable_dimension
  policy_type        = "StepScaling"
  step_scaling_policy_configuration {
    adjustment_type = "ExactCapacity"
    cooldown        = 60
    step_adjustment {
      scaling_adjustment          = each.value
      metric_interval_lower_bound = each.key == "lancer" ? 0 : null
      metric_interval_upper_bound = each.key == "lancer" ? null : 0
    }
  }
}

# Une tâche attend : on lance l'exécutant.
resource "aws_cloudwatch_metric_alarm" "worker_lancer" {
  count               = local.worker
  alarm_name          = "${local.name}-file-remplie"
  namespace           = "AWS/SQS"
  metric_name         = "ApproximateNumberOfMessagesVisible"
  dimensions          = { QueueName = module.socle.file_gpu_name }
  statistic           = "Maximum"
  period              = 60
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_appautoscaling_policy.worker["lancer"].arn]
}

# Plus rien en attente ni en cours depuis un quart d'heure : on l'arrête.
resource "aws_cloudwatch_metric_alarm" "worker_arreter" {
  count               = local.worker
  alarm_name          = "${local.name}-file-vide"
  evaluation_periods  = 15
  threshold           = 0
  comparison_operator = "LessThanOrEqualToThreshold"
  treat_missing_data  = "notBreaching"
  alarm_actions       = [aws_appautoscaling_policy.worker["arreter"].arn]
  metric_query {
    id          = "total"
    expression  = "attente + encours"
    label       = "Tâches en attente ou en cours"
    return_data = true
  }
  metric_query {
    id = "attente"
    metric {
      namespace   = "AWS/SQS"
      metric_name = "ApproximateNumberOfMessagesVisible"
      dimensions  = { QueueName = module.socle.file_gpu_name }
      stat        = "Maximum"
      period      = 60
    }
  }
  metric_query {
    id = "encours"
    metric {
      namespace   = "AWS/SQS"
      metric_name = "ApproximateNumberOfMessagesNotVisible"
      dimensions  = { QueueName = module.socle.file_gpu_name }
      stat        = "Maximum"
      period      = 60
    }
  }
}
