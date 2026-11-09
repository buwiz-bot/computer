output "region" {
  value = var.region
}

output "launch_template_id" {
  value = aws_launch_template.computer.id
}

output "launch_template_name" {
  value = aws_launch_template.computer.name
}

output "subnet_id" {
  value = local.subnet_id
}

output "security_group_id" {
  value = aws_security_group.computers.id
}

output "allowed_ssh_cidrs" {
  value = local.ssh_cidrs
}

output "ami_id" {
  value = data.aws_ami.ubuntu.id
}

# What the broker needs, for its environment.
output "broker_env" {
  value = <<-EOT
    BUWIZ_COMPUTER_PROVIDER=ec2
    BUWIZ_EC2_REGION=${var.region}
    BUWIZ_EC2_LAUNCH_TEMPLATE=${aws_launch_template.computer.name}
    BUWIZ_EC2_IMAGE=${var.image}:${var.image_tag}
    BUWIZ_EC2_PROFILE=buwiz-broker
  EOT
}
