<#
.SYNOPSIS
    Export the local Gather store and push an encrypted backup to the
    restic repository. Windows equivalent of gather-backup.sh — formalizes
    docs/TECHNICAL-WRITEUP.md §10 step 7 into a script Task Scheduler can
    invoke (see install-schedule-windows.ps1).

    This script is the ONLY thing that ever triggers the outbound VPS
    connection, and it only runs when a human or a human-installed
    scheduled task invokes it — never the daemon (§7.4/§7.5).

.PARAMETER GatherBaseUrl
    Default http://127.0.0.1:7601

.NOTES
    Required environment: RESTIC_REPOSITORY, RESTIC_PASSWORD (or
    RESTIC_PASSWORD_COMMAND). Optional: GATHER_API_TOKEN, GATHER_DAEMON_BIN,
    GATHER_BACKUP_LOG, RESTIC_KEEP_DAILY/WEEKLY/MONTHLY (default 7/4/6).
#>

param(
    [string]$GatherBaseUrl = $(if ($env:GATHER_BASE_URL) { $env:GATHER_BASE_URL } else { "http://127.0.0.1:7601" })
)

$ErrorActionPreference = "Stop"

$daemonBin = if ($env:GATHER_DAEMON_BIN) { $env:GATHER_DAEMON_BIN } else { "gather-daemon" }
$keepDaily = if ($env:RESTIC_KEEP_DAILY) { $env:RESTIC_KEEP_DAILY } else { "7" }
$keepWeekly = if ($env:RESTIC_KEEP_WEEKLY) { $env:RESTIC_KEEP_WEEKLY } else { "4" }
$keepMonthly = if ($env:RESTIC_KEEP_MONTHLY) { $env:RESTIC_KEEP_MONTHLY } else { "6" }

if (-not $env:RESTIC_REPOSITORY) {
    Write-Error "gather-backup: RESTIC_REPOSITORY must be set"
    exit 1
}

$logDir = if ($env:GATHER_BACKUP_LOG) { Split-Path $env:GATHER_BACKUP_LOG } else { "$env:LOCALAPPDATA\Gather\Logs" }
$logPath = if ($env:GATHER_BACKUP_LOG) { $env:GATHER_BACKUP_LOG } else { "$logDir\backup.log" }
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

function Write-Log($message) {
    $timestamp = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    $line = "{0} {1}" -f $timestamp, $message
    Add-Content -Path $logPath -Value $line
    Write-Host $line
}

$bundle = Join-Path $env:TEMP "gather-bundle-$([guid]::NewGuid()).ndjson"

try {
    $token = $env:GATHER_API_TOKEN
    if (-not $token) {
        try {
            $token = (& $daemonBin print-api-token 2>$null)
        } catch {
            $token = $null
        }
    }

    $headers = @{}
    if ($token) {
        $headers["Authorization"] = "Bearer $token"
    }

    Write-Log "backup: exporting from $GatherBaseUrl"
    try {
        Invoke-WebRequest -Uri "$GatherBaseUrl/api/v1/export" -Headers $headers -OutFile $bundle -UseBasicParsing
    } catch {
        Write-Log "backup: FAILED - export request failed: $_"
        exit 1
    }

    $bundleSize = (Get-Item $bundle).Length
    if ($bundleSize -eq 0) {
        Write-Log "backup: FAILED - export returned an empty bundle"
        exit 1
    }

    restic snapshots *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Log "backup: repository not yet initialized, running restic init"
        restic init
        if ($LASTEXITCODE -ne 0) {
            Write-Log "backup: FAILED - restic init failed"
            exit 1
        }
    }

    restic backup $bundle --tag gather-bundle
    if ($LASTEXITCODE -ne 0) {
        Write-Log "backup: FAILED - restic backup failed"
        exit 1
    }

    $snapshotJson = restic snapshots --latest 1 --json | ConvertFrom-Json
    $snapshotId = if ($snapshotJson) { $snapshotJson[0].short_id } else { "unknown" }

    restic forget --keep-daily $keepDaily --keep-weekly $keepWeekly --keep-monthly $keepMonthly --prune
    if ($LASTEXITCODE -ne 0) {
        Write-Log "backup: WARNING - snapshot $snapshotId taken but retention pruning failed"
        exit 1
    }

    Write-Log "backup: OK - snapshot $snapshotId ($bundleSize bytes)"
}
finally {
    if (Test-Path $bundle) {
        Remove-Item -Force $bundle
    }
}
