output "server_ipv4" {
  description = "Public IPv4 of the backup VM (restic/SSH target)."
  value       = hcloud_server.backup.ipv4_address
}

output "server_ipv6" {
  description = "Public IPv6 of the backup VM."
  value       = hcloud_server.backup.ipv6_address
}

output "restic_repository" {
  description = "Repository URL for restic on the local machine (after the one-time volume init)."
  value       = "sftp:gatherbackup@${hcloud_server.backup.ipv4_address}:/srv/backups/restic"
}

output "monthly_cost_estimate_eur" {
  description = "List-price estimate (server + volume + IPv4), EUR/month."
  value       = format("%.2f", 3.79 + (var.backup_volume_size_gb * 0.048) + 0.50)
}
