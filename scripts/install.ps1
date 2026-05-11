param(
  [string]$Prefix = $env:KODA_AGENT_PREFIX,
  [string]$Repo = $env:KODA_AGENT_REPO,
  [string]$Version = $(if ($env:KODA_AGENT_VERSION) { $env:KODA_AGENT_VERSION } else { 'latest' }),
  [switch]$FromSource,
  [switch]$BootstrapPython,
  [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA 'koda-agent' }
$BinDir = Join-Path $Prefix 'bin'
$DataDir = if ($env:KODA_AGENT_HOME) { $env:KODA_AGENT_HOME } else { Join-Path $env:USERPROFILE '.koda-agent' }

function Invoke-Step {
  param([scriptblock]$Block, [string]$Text)
  if ($DryRun) { Write-Host "+ $Text" } else { & $Block }
}

function Copy-KodaResources {
  param([string]$Source)
  if (-not (Test-Path $Source)) { return }
  $exe = Join-Path $BinDir 'koda-agent.exe'
  if (Test-Path $exe) {
    Invoke-Step { & $exe resources install --source $Source --repair } "$exe resources install --source $Source --repair"
  } else {
    Invoke-Step { koda-agent resources install --source $Source --repair } "koda-agent resources install --source $Source --repair"
  }
}

function Initialize-KodaConfig {
  $exe = Join-Path $BinDir 'koda-agent.exe'
  $sourceEnv = Join-Path (Get-Location).Path '.env'
  $hasSourceEnv = Test-Path $sourceEnv
  if (Test-Path $exe) {
    if ($hasSourceEnv) {
      Invoke-Step { & $exe init --from-env $sourceEnv } "$exe init --from-env $sourceEnv"
    } else {
      Invoke-Step { & $exe init } "$exe init"
    }
  } elseif (Get-Command koda-agent -ErrorAction SilentlyContinue) {
    if ($hasSourceEnv) {
      Invoke-Step { koda-agent init --from-env $sourceEnv } "koda-agent init --from-env $sourceEnv"
    } else {
      Invoke-Step { koda-agent init } 'koda-agent init'
    }
  }
}

if ($FromSource) {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Missing required command: cargo' }
  Invoke-Step { cargo install --path crates/koda-agent-cli --locked --root $Prefix --force } "cargo install --path crates/koda-agent-cli --locked --root $Prefix --force"
  Copy-KodaResources (Get-Location).Path
} else {
  if (-not $Repo) { throw 'No release repository configured. Use -Repo OWNER/REPO, set KODA_AGENT_REPO, or pass -FromSource.' }
  $arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
  switch ($arch) {
    'x64' { $target = 'x86_64-pc-windows-msvc' }
    'arm64' { $target = 'aarch64-pc-windows-msvc' }
    default { throw "Unsupported Windows architecture: $arch" }
  }
  if ($Version -eq 'latest') {
    $url = "https://github.com/$Repo/releases/latest/download/koda-agent-$target.zip"
    $sumUrl = "https://github.com/$Repo/releases/latest/download/SHA256SUMS"
  } else {
    $url = "https://github.com/$Repo/releases/download/$Version/koda-agent-$target.zip"
    $sumUrl = "https://github.com/$Repo/releases/download/$Version/SHA256SUMS"
  }
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("koda-agent-install-" + [Guid]::NewGuid())
  Invoke-Step { New-Item -ItemType Directory -Force -Path $tmp, $BinDir, $DataDir | Out-Null } "create $tmp, $BinDir, $DataDir"
  $zip = Join-Path $tmp 'koda-agent.zip'
  Invoke-Step { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip } "download $url"
  if (-not $DryRun) {
    try {
      $sumFile = Join-Path $tmp 'SHA256SUMS'
      Invoke-WebRequest -UseBasicParsing -Uri $sumUrl -OutFile $sumFile
      $expected = Select-String -Path $sumFile -Pattern "koda-agent-$target.zip" | Select-Object -First 1
      if ($expected) {
        $want = ($expected.Line -split '\s+')[0].ToLowerInvariant()
        $got = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
        if ($want -ne $got) { throw "Checksum mismatch: expected $want got $got" }
      }
    } catch {
      Write-Warning "Checksum verification skipped: $($_.Exception.Message)"
    }
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    Copy-Item -Force (Join-Path $tmp 'koda-agent.exe') (Join-Path $BinDir 'koda-agent.exe')
    Copy-KodaResources (Join-Path $tmp 'resources')
    Remove-Item -Recurse -Force $tmp
  }
}

Invoke-Step { New-Item -ItemType Directory -Force -Path $DataDir | Out-Null } "create $DataDir"
Initialize-KodaConfig
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($UserPath -notlike "*$BinDir*") {
  Invoke-Step { [Environment]::SetEnvironmentVariable('Path', "$UserPath;$BinDir", 'User') } "add $BinDir to user PATH"
}

if ($BootstrapPython) {
  $exe = Join-Path $BinDir 'koda-agent.exe'
  if (Test-Path $exe) {
    Invoke-Step { & $exe bootstrap-python --extras core --repair } "$exe bootstrap-python --extras core --repair"
  } else {
    Invoke-Step { koda-agent bootstrap-python --extras core --repair } 'koda-agent bootstrap-python --extras core --repair'
  }
}

Write-Host "Installed koda-agent under $Prefix"
Write-Host 'Open a new terminal, then run: koda-agent doctor'
