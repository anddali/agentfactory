$projectRoot = Split-Path -Parent $PSScriptRoot
$envPath = Join-Path $projectRoot '.env'
if (-not (Test-Path -LiteralPath $envPath)) { throw 'Run scripts/setup.ps1 first.' }
foreach ($line in [IO.File]::ReadAllLines($envPath)) {
    if ($line -match '^([A-Z_]+)=(.*)$') {
        $key = $Matches[1]
        $value = $Matches[2].Trim("'")
        [Environment]::SetEnvironmentVariable($key, $value, 'Process')
    }
}
