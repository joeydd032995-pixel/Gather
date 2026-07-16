#!/usr/bin/env bash
# install-schedule-macos.sh — install a launchd LaunchAgent that runs
# gather-backup.sh on a schedule. Idempotent (bootout-then-bootstrap).
#
# The daemon itself never schedules anything (§7.4/§7.5) — this installs a
# separate, user-owned LaunchAgent that calls the backup script directly;
# Gather's own process is never involved in triggering it.
#
# Usage:
#   RESTIC_REPOSITORY=... RESTIC_PASSWORD=... \
#     scripts/install-schedule-macos.sh [--hour 3] [--minute 0]
set -euo pipefail

hour=3
minute=0
while [ $# -gt 0 ]; do
  case "$1" in
    --hour)
      hour="$2"
      shift 2
      ;;
    --minute)
      minute="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [ -z "${RESTIC_REPOSITORY:-}" ]; then
  echo "install-schedule-macos: RESTIC_REPOSITORY must be set (same value gather-backup.sh needs)" >&2
  exit 1
fi
if [ -z "${RESTIC_PASSWORD:-}" ] && [ -z "${RESTIC_PASSWORD_COMMAND:-}" ]; then
  echo "install-schedule-macos: RESTIC_PASSWORD or RESTIC_PASSWORD_COMMAND must be set" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
backup_script="$script_dir/gather-backup.sh"
label="com.gather.backup"
plist_dir="$HOME/Library/LaunchAgents"
mkdir -p "$plist_dir"
plist_path="$plist_dir/$label.plist"

log_dir="$HOME/Library/Logs/Gather"
mkdir -p "$log_dir"

cat >"$plist_path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$label</string>
    <key>ProgramArguments</key>
    <array>
        <string>$backup_script</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RESTIC_REPOSITORY</key>
        <string>$RESTIC_REPOSITORY</string>
$( [ -n "${RESTIC_PASSWORD:-}" ] && printf '        <key>RESTIC_PASSWORD</key>\n        <string>%s</string>\n' "$RESTIC_PASSWORD" )
$( [ -n "${RESTIC_PASSWORD_COMMAND:-}" ] && printf '        <key>RESTIC_PASSWORD_COMMAND</key>\n        <string>%s</string>\n' "$RESTIC_PASSWORD_COMMAND" )
$( [ -n "${GATHER_API_TOKEN:-}" ] && printf '        <key>GATHER_API_TOKEN</key>\n        <string>%s</string>\n' "$GATHER_API_TOKEN" )
$( [ -n "${GATHER_BASE_URL:-}" ] && printf '        <key>GATHER_BASE_URL</key>\n        <string>%s</string>\n' "$GATHER_BASE_URL" )
    </dict>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>$hour</integer>
        <key>Minute</key>
        <integer>$minute</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>$log_dir/backup.out.log</string>
    <key>StandardErrorPath</key>
    <string>$log_dir/backup.err.log</string>
</dict>
</plist>
EOF
chmod 600 "$plist_path"

uid="$(id -u)"
launchctl bootout "gui/$uid/$label" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$plist_path"

echo "installed: $label (daily at ${hour}:$(printf '%02d' "$minute"))"
echo "check status: launchctl print gui/$uid/$label"
echo "check logs:   $log_dir/backup.err.log"
