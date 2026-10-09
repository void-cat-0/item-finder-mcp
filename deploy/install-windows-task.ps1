<#
.SYNOPSIS
  Register the where-i-put resident ingest daemon as a Windows scheduled task.

.DESCRIPTION
  §8.4's recommended Windows shape: Task Scheduler, no external service
  wrapper, no admin requirement for a logon-triggered task. The task runs the
  daemon under a console so the console control events (Ctrl-C / Ctrl-Break /
  console close / logoff / shutdown) reach the daemon's handlers -- those are
  Windows' only graceful stop path, and the daemon drains cameras on them.

  Restart-on-failure is configured to match §8's "two levels of supervision,
  each independent": Task Scheduler restarts the whole daemon, the daemon's
  own Supervisor restarts individual cameras. Neither is left to the other.

.PARAMETER ExePath
  Full path to item-ingest.exe. Defaults to the release build in this repo.

.PARAMETER ConfigPath
  Full path to the deployment's config.toml.

.PARAMETER TaskName
  Task name to register under. Default: where-i-put ingest.

.PARAMETER AtStartup
  Start at system boot instead of user logon. Requires an elevated shell and a
  service account that can read the camera.

.EXAMPLE
  # Register for the current user, starting at logon:
  powershell -ExecutionPolicy Bypass -File deploy\install-windows-task.ps1 `
      -ExePath C:\where-i-put\item-ingest.exe `
      -ConfigPath C:\where-i-put\config.toml

.EXAMPLE
  # Remove it again:
  powershell -ExecutionPolicy Bypass -File deploy\install-windows-task.ps1 -Unregister
#>
[CmdletBinding()]
param(
    [string]$ExePath = "$PSScriptRoot\..\target\release\item-ingest.exe",
    [string]$ConfigPath = "$PSScriptRoot\..\config.toml",
    [string]$TaskName = "where-i-put ingest",
    [switch]$AtStartup,
    [switch]$Unregister
)

$ErrorActionPreference = "Stop"

if ($Unregister) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "unregistered task '$TaskName'"
    exit 0
}

# Check the inputs now: Task Scheduler will happily register a task pointing at
# a path that does not exist, and the failure then shows up only at the next
# logon.
if (-not (Test-Path -LiteralPath $ExePath)) { throw "exe not found: $ExePath" }
if (-not (Test-Path -LiteralPath $ConfigPath)) { throw "config not found: $ConfigPath" }
$ExePath = (Resolve-Path -LiteralPath $ExePath).Path
$ConfigPath = (Resolve-Path -LiteralPath $ConfigPath).Path

# The daemon resolves relative config paths against the CONFIG FILE's directory
# (§3), so the working directory is mostly irrelevant -- but set it to the
# config's directory anyway, which is what a human running this by hand would do
# (and what makes `models/yolov8n.onnx` resolve as written in the README).
$WorkDir = Split-Path -Parent $ConfigPath

# Run through cmd.exe so the process owns a console: that is what lets
# Ctrl-Break and a console close reach the daemon. A bare exe launched by the
# Task Scheduler has no console, and then only the shutdown reasons that
# Windows delivers regardless (logoff / shutdown) would get through.
$Action = New-ScheduledTaskAction -Execute $ExePath `
    -Argument "--daemon --config `"$ConfigPath`"" `
    -WorkingDirectory $WorkDir

$Trigger = if ($AtStartup) {
    New-ScheduledTaskTrigger -AtStartup
} else {
    New-ScheduledTaskTrigger -AtLogOn
}

# Restart three times at one-minute intervals, then stop trying: a task that
# restarts forever can hide a configuration error behind log noise (§8.4).
$Settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero)   # no time limit: this runs for weeks

# Interactive is the default; a background task must be able to read the camera.
$Principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
    -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName `
    -Action $Action -Trigger $Trigger -Settings $Settings -Principal $Principal `
    -Force | Out-Null

Write-Host "registered task '$TaskName'"
Write-Host "  exe:    $ExePath"
Write-Host "  config: $ConfigPath"
Write-Host "  start:  $(if ($AtStartup) { 'at system startup' } else { 'at user logon' })"
Write-Host ""
Write-Host "Start it now:   Start-ScheduledTask -TaskName '$TaskName'"
Write-Host "Check status:   Get-ScheduledTaskInfo -TaskName '$TaskName'"
Write-Host "Stop it:        Stop-ScheduledTask  -TaskName '$TaskName'   (a hard stop)"
Write-Host "Remove:         deploy\install-windows-task.ps1 -Unregister"
Write-Host ""
Write-Host "Note: Stop-ScheduledTask ends the process without a console event, so the"
Write-Host "daemon cannot drain; the next start takes over the resulting stale lock"
Write-Host "(design/resident-ingest.md §8). To stop cleanly, close its console window or"
Write-Host "send Ctrl-C from the interactive session that started it."