variable "region" {
  description = "Where computers run. The broker's BUWIZ_EC2_REGION must say the same."
  type        = string
  default     = "ap-southeast-1"
}

variable "name" {
  description = "The launch template's name, and the prefix of everything else. The broker's BUWIZ_EC2_LAUNCH_TEMPLATE must say the same."
  type        = string
  default     = "buwiz-computer"
}

variable "instance_type" {
  description = "A type that can hibernate, with under 150 GiB of memory. m7i.large is 2 vCPU and 8 GiB."
  type        = string
  default     = "m7i.large"
}

variable "image" {
  description = "The box image the instance pulls at its first boot. The broker's BUWIZ_EC2_IMAGE should say the same, or the first wake pulls it again."
  type        = string
  default     = "ghcr.io/buwiz-bot/computer"
}

variable "image_tag" {
  description = "Its tag."
  type        = string
  default     = "latest"
}

variable "root_volume_gib" {
  description = "The encrypted gp3 root volume: the OS, Docker and the image, the computer's files, and room for all of its memory when it hibernates."
  type        = number
  default     = 60

  validation {
    condition     = var.root_volume_gib >= 30
    error_message = "Hibernation writes the instance's whole memory to the root volume: give it at least 30 GiB."
  }
}

variable "hibernation" {
  description = "Launch able to hibernate. Off, sleep is a plain stop and what was open is gone at the next wake."
  type        = bool
  default     = true
}

variable "kms_key_id" {
  description = "A KMS key for the root volume. Null is the account's default EBS key."
  type        = string
  default     = null
}

variable "allowed_ssh_cidrs" {
  description = "Who may reach port 22: the Mac running the broker. Empty is this machine's public address now, as checkip.amazonaws.com sees it."
  type        = list(string)
  default     = []
}

variable "vpc_cidr" {
  description = "The VPC made when no subnet_id is given."
  type        = string
  default     = "10.73.0.0/16"
}

variable "subnet_id" {
  description = "A public subnet to use instead of making a VPC. It must give instances a public IPv4 address. It is tagged buwiz:managed=true, which the broker's policy needs."
  type        = string
  default     = null
}

variable "availability_zone" {
  description = "Where the made subnet goes. Null is the region's first zone."
  type        = string
  default     = null
}

variable "local_zone" {
  description = "A Local Zone for the made subnet, e.g. ap-southeast-1-mnl-1a (Manila). Its zone group is opted in. Few types run there: set instance_type to one that does, and check that it can hibernate."
  type        = string
  default     = null
}

variable "enable_ssm" {
  description = "Give instances a role with SSM Session Manager (AmazonSSMManagedInstanceCore). Off, they have no role at all."
  type        = bool
  default     = false
}
