#!/usr/bin/env bash
# install-schedule-linux.sh — install a systemd --user timer that runs
# gather-backup.sh on a schedule. Idempotent (safe to re-run to change the
# schedule or environment).
#
# The daemon itself never schedules anything (§7.4/§7.5) — this installs a
# separate, user-owned systemd unit that calls the backup script directly;
# Gather's own process is never involved in triggering it.
#
# Usage:
#   RESTIC_REPOSITORY=... RESTIC_PASSWORD=... \
#     scripts/install-schedule-linux.sh [--on-calendar "daily"]
#
# Non-systemd distros: see docs/BACKUP-RUNBOOK.md for a documented crontab
# line instead — this script assumes systemd --user is available.
set -euo pipefail

on_calendar="daily"
while [ $# -gt 0 ]; do
  case "$1" in
    --on-calendar)
      on_calendar="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if ! command -v systemctl >/dev/null 2>&1; then
  echo "install-schedule-linux: systemctl not found; see docs/BACKUP-RUNBOOK.md for the crontab fallback" >&2
  exit 1
fi

if [ -z "${RESTIC_REPOSITORY:-}" ]; then
  echo "install-schedule-linux: RESTIC_REPOSITORY must be set (same value gather-backup.sh needs)" >&2
  exit 1
fi
if [ -z "${RESTIC_PASSWORD:-}" ] && [ -z "${RESTIC_PASSWORD_COMMAND:-}" ]; then
  echo "install-schedule-linux: RESTIC_PASSWORD or RESTIC_PASSWORD_COMMAND must be set" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
backup_script="$script_dir/gather-backup.sh"
unit_dir="$HOME/.config/systemd/user"
mkdir -p "$unit_dir"

# EnvironmentFile keeps secrets out of the unit file itself (0600 perms).
env_file="${XDG_STATE_HOME:-$HOME/.local/state}/gather/backup.env"
mkdir -p "$(dirname "$env_file")"
{
  echo "RESTIC_REPOSITORY=$RESTIC_REPOSITORY"
  [ -n "${RESTIC_PASSWORD:-}" ] && echo "RESTIC_PASSWORD=$RESTIC_PASSWORD"
  [ -n "${RESTIC_PASSWORD_COMMAND:-}" ] && echo "RESTIC_PASSWORD_COMMAND=$RESTIC_PASSWORD_COMMAND"
  [ -n "${GATHER_API_TOKEN:-}" ] && echo "GATHER_API_TOKEN=$GATHER_API_TOKEN"
  [ -n "${GATHER_BASE_URL:-}" ] && echo "GATHER_BASE_URL=$GATHER_BASE_URL"
} >"$env_file"
chmod 600 "$env_file"

cat >"$unit_dir/gather-backup.service" <<EOF
[Unit]
Description=Gather encrypted export backup

[Service]
Type=oneshot
EnvironmentFile=$env_file
ExecStart=$backup_script
EOF

cat >"$unit_dir/gather-backup.timer" <<EOF
[Unit]
Description=Run gather-backup.service on a schedule

[Timer]
OnCalendar=$on_calendar
Persistent=true

[Install]
WantedBy=timers.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now gather-backup.timer

# The timer must fire even when the user isn't in an active graphical
# session (e.g. after logout on a headless/server-like desktop). Without
# this, systemd --user units stop when the session ends.
if command -v loginctl >/dev/null 2>&1; then
  loginctl enable-linger "$(whoami)" || true
fi

echo "installed: gather-backup.timer (OnCalendar=$on_calendar)"
echo "check status: systemctl --user status gather-backup.timer"
echo "check logs:   journalctl --user -u gather-backup.service"
