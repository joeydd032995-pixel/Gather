<#
.SYNOPSIS
    Install a Windows Task Scheduler task that runs gather-backup.ps1 on a
    schedule. Idempotent (re-registers with -Force).

    The daemon itself never schedules anything (§7.4/§7.5) — this creates a
    separate, user-owned scheduled task that calls the backup script
    directly; Gather's own process is never involved in triggering it.

.PARAMETER At
    Time of day to run, e.g. "03:00". Default "03:00".

.NOTES
    Set RESTIC_REPOSITORY / RESTIC_PASSWORD (or RESTIC_PASSWORD_COMMAND) as
    user-level environment variables before running this (e.g. via
    [Environment]::SetEnvironmentVariable(..., "User")) — the scheduled task
    runs as the current user and inherits them from there.

    Registered with -RunLevel Limited and an interactive logon trigger so
    the task runs in the same session context as the user who set up
    Windows Credential Manager access for GATHER_AUTH_MODE=keychain. Running
    "whether the user is logged on or not" needs a separately stored
    credential and is out of scope here — see docs/BACKUP-RUNBOOK.md.
#>

param(
    [string]$At = "03:00"
)

$ErrorActionPreference = "Stop"

if (-not $env:RESTIC_REPOSITORY) {
    Write-Error "install-schedule-windows: RESTIC_REPOSITORY must be set as a user environment variable"
    exit 1
}
if (-not $env:RESTIC_PASSWORD -and -not $env:RESTIC_PASSWORD_COMMAND) {
    Write-Error "install-schedule-windows: RESTIC_PASSWORD or RESTIC_PASSWORD_COMMAND must be set as a user environment variable"
    exit 1
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$backupScript = Join-Path $scriptDir "gather-backup.ps1"
$taskName = "GatherBackup"

$action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$backupScript`""
$trigger = New-ScheduledTaskTrigger -Daily -At $At
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopOnIdleEnd

Register-ScheduledTask -TaskName $taskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -RunLevel Limited `
    -Force | Out-Null

Write-Host "installed: scheduled task '$taskName' (daily at $At)"
Write-Host "check status: Get-ScheduledTaskInfo -TaskName $taskName"
Write-Host "check logs:   `$env:LOCALAPPDATA\Gather\Logs\backup.log"
