terraform {
  required_version = ">= 1.5"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.70, < 7.0"
    }
    http = {
      source  = "hashicorp/http"
      version = "~> 3.4"
    }
  }
}

# Credentials come from the AWS chain: `AWS_PROFILE=buwiz tofu apply`. Never in a .tf file.
provider "aws" {
  region = var.region

  default_tags {
    tags = {
      "buwiz:stack" = var.name
    }
  }
}
