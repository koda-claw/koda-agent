param(
  [string]$Prefix = $env:KODA_AGENT_PREFIX,
  [switch]$RemoveData,
  [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA 'koda-agent' }
$BinDir = Join-Path $Prefix 'bin'
$DataDir = if ($env:KODA_AGENT_HOME) { $env:KODA_AGENT_HOME } else { Join-Path $env:USERPROFILE '.koda-agent' }
function Invoke-Step { param([scriptblock]$Block, [string]$Text) if ($DryRun) { Write-Host "+ $Text" } else { & $Block } }
Invoke-Step { Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $BinDir 'koda-agent.exe') } "remove $(Join-Path $BinDir 'koda-agent.exe')"
if ($RemoveData) { Invoke-Step { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $DataDir } "remove $DataDir" }
Write-Host 'Uninstall complete.'
