param(
  [string]$Prefix = $env:KODA_AGENT_PREFIX,
  [string]$Repo = $env:KODA_AGENT_REPO,
  [string]$Version = $(if ($env:KODA_AGENT_VERSION) { $env:KODA_AGENT_VERSION } else { 'latest' }),
  [switch]$FromSource,
  [switch]$BootstrapPython,
  [switch]$DryRun
)
& (Join-Path $PSScriptRoot 'install.ps1') -Prefix $Prefix -Repo $Repo -Version $Version -FromSource:$FromSource -BootstrapPython:$BootstrapPython -DryRun:$DryRun
