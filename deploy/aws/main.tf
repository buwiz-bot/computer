# One launch template the broker launches every computer from, and the network it lands in.
# The broker launches, stops and terminates the instances; nothing here makes one.

locals {
  make_network = var.subnet_id == null
  ssh_cidrs    = length(var.allowed_ssh_cidrs) > 0 ? var.allowed_ssh_cidrs : ["${chomp(data.http.my_ip[0].response_body)}/32"]
  zone         = var.local_zone != null ? var.local_zone : (var.availability_zone != null ? var.availability_zone : data.aws_availability_zones.here.names[0])
  subnet_id    = local.make_network ? aws_subnet.computers[0].id : var.subnet_id
  vpc_id       = local.make_network ? aws_vpc.computers[0].id : data.aws_subnet.given[0].vpc_id

  # The broker's policy lets it launch only into a subnet, security group and launch template with
  # this tag, and only images of the account the AMI lookup asks for.
  managed   = { "buwiz:managed" = "true" }
  ami_owner = "099720109477"
}

# The public address this is applied from, when no CIDR is given: the Mac the broker runs on, as a rule.
data "http" "my_ip" {
  count = length(var.allowed_ssh_cidrs) > 0 ? 0 : 1
  url   = "https://checkip.amazonaws.com"
}

data "aws_availability_zones" "here" {
  state = "available"
}

data "aws_subnet" "given" {
  count = local.make_network ? 0 : 1
  id    = var.subnet_id
}

# A given subnet gets the tag too, or the broker may not launch into it.
resource "aws_ec2_tag" "given_subnet" {
  count       = local.make_network ? 0 : 1
  resource_id = var.subnet_id
  key         = "buwiz:managed"
  value       = "true"
}

# Ubuntu 24.04 LTS, from Canonical, for the instance type's architecture.
data "aws_ec2_instance_type" "chosen" {
  instance_type = var.instance_type
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = [local.ami_owner]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${contains(data.aws_ec2_instance_type.chosen.supported_architectures, "arm64") ? "arm64" : "amd64"}-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# ---- network: a VPC with one public subnet, unless one is given ----

resource "aws_ec2_availability_zone_group" "local_zone" {
  count         = var.local_zone != null ? 1 : 0
  group_name    = regex("^(.*[0-9])[a-z]$", var.local_zone)[0]
  opt_in_status = "opted-in"
}

resource "aws_vpc" "computers" {
  count                = local.make_network ? 1 : 0
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = { Name = var.name }
}

resource "aws_internet_gateway" "computers" {
  count  = local.make_network ? 1 : 0
  vpc_id = aws_vpc.computers[0].id

  tags = { Name = var.name }
}

resource "aws_subnet" "computers" {
  count                   = local.make_network ? 1 : 0
  vpc_id                  = aws_vpc.computers[0].id
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, 1)
  availability_zone       = local.zone
  map_public_ip_on_launch = true

  tags = merge({ Name = var.name }, local.managed)

  depends_on = [aws_ec2_availability_zone_group.local_zone]
}

resource "aws_route_table" "computers" {
  count  = local.make_network ? 1 : 0
  vpc_id = aws_vpc.computers[0].id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.computers[0].id
  }

  tags = { Name = var.name }
}

resource "aws_route_table_association" "computers" {
  count          = local.make_network ? 1 : 0
  subnet_id      = aws_subnet.computers[0].id
  route_table_id = aws_route_table.computers[0].id
}

# SSH from the broker's Mac, and nothing else in. The box's ports are on the instance's loopback.
resource "aws_security_group" "computers" {
  name        = var.name
  description = "Buwiz computers: SSH from the broker, nothing else in"
  vpc_id      = local.vpc_id

  tags = merge({ Name = var.name }, local.managed)
}

resource "aws_vpc_security_group_ingress_rule" "ssh" {
  for_each          = toset(local.ssh_cidrs)
  security_group_id = aws_security_group.computers.id
  description       = "SSH from the broker"
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = each.value
}

resource "aws_vpc_security_group_egress_rule" "out" {
  security_group_id = aws_security_group.computers.id
  description       = "The box browses the internet"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

# ---- an optional role: SSM Session Manager, and nothing else ----

resource "aws_iam_role" "instance" {
  count = var.enable_ssm ? 1 : 0
  # the broker may pass only roles named buwiz-computer*
  name = "${var.name}-instance"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "ssm" {
  count      = var.enable_ssm ? 1 : 0
  role       = aws_iam_role.instance[0].name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "instance" {
  count = var.enable_ssm ? 1 : 0
  name  = "${var.name}-instance"
  role  = aws_iam_role.instance[0].name
}

# ---- the launch template ----

resource "aws_launch_template" "computer" {
  name                   = var.name
  description            = "A Buwiz Bot computer: Ubuntu 24.04, Docker and the box image, able to hibernate"
  image_id               = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  update_default_version = true
  tags                   = local.managed
  user_data              = base64encode(templatefile("${path.module}/user-data.yaml", { image = "${var.image}:${var.image_tag}" }))

  block_device_mappings {
    device_name = data.aws_ami.ubuntu.root_device_name

    ebs {
      volume_size           = var.root_volume_gib
      volume_type           = "gp3"
      encrypted             = true
      kms_key_id            = var.kms_key_id
      delete_on_termination = true
    }
  }

  hibernation_options {
    configured = var.hibernation
  }

  # IMDSv2 only, one hop: a container on the instance can't get a token
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
    instance_metadata_tags      = "enabled"
  }

  network_interfaces {
    device_index                = 0
    subnet_id                   = local.subnet_id
    security_groups             = [aws_security_group.computers.id]
    associate_public_ip_address = true
    delete_on_termination       = true
  }

  dynamic "iam_instance_profile" {
    for_each = var.enable_ssm ? [1] : []
    content {
      name = aws_iam_instance_profile.instance[0].name
    }
  }

  depends_on = [aws_route_table_association.computers]
}

# The account tofu runs as, for the broker's policy (`broker_policy_json`).
data "aws_caller_identity" "current" {}
