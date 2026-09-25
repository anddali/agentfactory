$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$envPath = Join-Path $projectRoot '.env'
if (Test-Path -LiteralPath $envPath) { Write-Host '.env already exists; preserving its credentials.'; exit 0 }
function New-Secret { $bytes = New-Object byte[] 32; [Security.Cryptography.RandomNumberGenerator]::Fill($bytes); return [Convert]::ToHexString($bytes).ToLowerInvariant() }
$password = New-Secret
$workerSecret = New-Secret
$apiToken = New-Secret
$identities = @(@{ token = $apiToken; subject = 'local-maintainer'; roles = @('observer', 'operator', 'approver', 'connector_admin'); repositories = @('*') }) | ConvertTo-Json -Compress -AsArray
$content = "POSTGRES_PASSWORD=$password`nFACTORY_WORKER_SECRET=$workerSecret`nFACTORY_IDENTITIES='$identities'`n"
[IO.File]::WriteAllText($envPath, $content)
Write-Host 'Created local credentials in .env. Start with docker compose --profile build build, then docker compose up -d.'
