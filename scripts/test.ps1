$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/env.ps1"
Push-Location $projectRoot
try {
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed.' }
    $exists = docker compose exec -T db psql -U factory -d factory -tAc "SELECT 1 FROM pg_database WHERE datname='factory_test'"
    if ($LASTEXITCODE -ne 0) { throw 'Start PostgreSQL with docker compose up -d db.' }
    if ($exists -ne '1') { docker compose exec -T db createdb -U factory factory_test }
    $env:DATABASE_URL = "postgres://factory:$($env:POSTGRES_PASSWORD)@127.0.0.1:54329/factory_test"
    cargo test --test database -- --ignored
    if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL integration tests failed.' }
    node --check web/app.js
    if ($LASTEXITCODE -ne 0) { throw 'Portal JavaScript check failed.' }
    node --test tests/portal-decisions.cjs
    if ($LASTEXITCODE -ne 0) { throw 'Portal decision tests failed.' }
} finally { Pop-Location }
