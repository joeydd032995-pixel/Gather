# =============================================================================
# Gather — OPTIONAL encrypted backup/replication target (opt-in only)
#
# One Hetzner CX22 VM + one detachable volume, receiving client-side-encrypted
# restic backups of Gather export bundles over SSH. The VM never sees
# plaintext data: restic encrypts with AES-256 before bytes leave the local
# machine, and the volume is additionally LUKS-encrypted at rest.
#
# Monthly cost assumptions (Hetzner list prices, EUR, 2026):
#   - CX22 (2 vCPU shared, 4 GB RAM, 40 GB NVMe):  EUR 3.79 / month
#   - 20 GB volume (EUR 0.048 / GB / month):        EUR 0.96 / month
#   - IPv4 primary IP:                              EUR 0.50 / month
#   Total: ~EUR 5.25 / month  (~USD 6) — well under the USD 75 ceiling.
# =============================================================================

resource "hcloud_ssh_key" "admin" {
  name       = "gather-backup-admin"
  public_key = var.ssh_public_key
}

resource "hcloud_firewall" "backup" {
  name = "gather-backup-fw"

  # SSH from the admin CIDR only — restic runs over this same SSH transport.
  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "22"
    source_ips = [var.admin_cidr]
  }

  # ICMP for basic reachability checks from the admin network.
  rule {
    direction  = "in"
    protocol   = "icmp"
    source_ips = [var.admin_cidr]
  }

  # Everything else inbound is dropped by Hetzner's default-deny.
  # No outbound rules: outbound stays open solely for apt security updates;
  # the VM initiates no connections back to the local machine.
}

resource "hcloud_volume" "backup" {
  name     = "gather-backup-vol"
  size     = var.backup_volume_size_gb
  location = var.location
  format   = null # formatted as LUKS by cloud-init, not by Hetzner
}

resource "hcloud_server" "backup" {
  name        = "gather-backup"
  server_type = var.server_type
  image       = "ubuntu-24.04"
  location    = var.location

  ssh_keys     = [hcloud_ssh_key.admin.id]
  firewall_ids = [hcloud_firewall.backup.id]

  user_data = file("${path.module}/cloud-init.yaml")

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = {
    project = "gather"
    role    = "backup-target"
    opt_in  = "true"
  }
}

resource "hcloud_volume_attachment" "backup" {
  volume_id = hcloud_volume.backup.id
  server_id = hcloud_server.backup.id
  automount = false # cloud-init handles LUKS + mount
}
