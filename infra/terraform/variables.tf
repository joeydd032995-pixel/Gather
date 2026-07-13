variable "hcloud_token" {
  description = "Hetzner Cloud API token. Prefer HCLOUD_TOKEN env var (TF_VAR_hcloud_token) over tfvars files; never commit it."
  type        = string
  sensitive   = true
}

variable "ssh_public_key" {
  description = "OpenSSH public key granted access to the backup VM (contents of ~/.ssh/id_ed25519.pub)."
  type        = string
}

variable "admin_cidr" {
  description = "CIDR allowed to reach SSH (e.g. your home IP as x.x.x.x/32). 0.0.0.0/0 works but weakens hardening — set your real IP."
  type        = string

  validation {
    condition     = can(cidrhost(var.admin_cidr, 0))
    error_message = "admin_cidr must be a valid IPv4 CIDR, e.g. 203.0.113.7/32."
  }
}

variable "location" {
  description = "Hetzner location (nbg1 = Nuremberg, fsn1 = Falkenstein, hel1 = Helsinki)."
  type        = string
  default     = "nbg1"
}

variable "server_type" {
  description = "Instance size. cx22 (2 vCPU / 4 GB / 40 GB) is ample for a restic/rsync backup target."
  type        = string
  default     = "cx22"
}

variable "backup_volume_size_gb" {
  description = "Size of the detachable volume holding encrypted backups (LUKS on top; grow later without re-provisioning)."
  type        = number
  default     = 20

  validation {
    condition     = var.backup_volume_size_gb >= 10
    error_message = "Hetzner volumes start at 10 GB."
  }
}
