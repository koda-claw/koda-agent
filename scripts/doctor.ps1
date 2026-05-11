$ErrorActionPreference = 'Stop'
$cmd = Get-Command koda-agent -ErrorAction SilentlyContinue
if ($cmd) { & $cmd.Source doctor --json; exit $LASTEXITCODE }
if (Test-Path './target/debug/koda-agent.exe') { & './target/debug/koda-agent.exe' doctor --json; exit $LASTEXITCODE }
cargo run -q -p koda-agent-cli -- doctor --json
