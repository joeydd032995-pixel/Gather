# =============================================================================
# OPTIONAL / OPT-IN INFRASTRUCTURE
#
# Nothing in Gather requires this. The system is local-first and fully
# functional offline. Apply this configuration ONLY if you explicitly choose
# encrypted off-site replication of your export bundles (write-up §7.5).
# =============================================================================

terraform {
  required_version = ">= 1.6.0"

  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
  }
}

provider "hcloud" {
  # Export HCLOUD_TOKEN instead of writing the token to disk:
  #   export HCLOUD_TOKEN=$(security find-generic-password -s hcloud -w)  # macOS keychain
  #   export HCLOUD_TOKEN=$(secret-tool lookup service hcloud)            # Linux keyring
  token = var.hcloud_token
}
