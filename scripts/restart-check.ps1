$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/env.ps1"
$baseUrl = 'http://127.0.0.1:8787'
$identity = @($env:FACTORY_IDENTITIES | ConvertFrom-Json)[0]
$headers = @{Authorization="Bearer $($identity.token)";'Idempotency-Key'="restart-$([Guid]::NewGuid())"}
$request = @{workflow='demo';repository='local-demo';issue=@{provider='fixture';key="RESTART-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())";title='Review research after a control plane restart';body='Acceptance test: this approval must survive server replacement.'}} | ConvertTo-Json -Depth 5
$job = Invoke-RestMethod "$baseUrl/api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $request
$deadline = [DateTime]::UtcNow.AddSeconds(60)
do {
    Start-Sleep -Seconds 1
    $before = (Invoke-RestMethod "$baseUrl/api/jobs/$($job.id)" -Headers $headers).job
    if ($before.status -in @('failed','timed_out')) { throw "Job failed: $($before.attempts[-1].error)" }
} while ($before.status -ne 'awaiting_approval' -and [DateTime]::UtcNow -lt $deadline)
if ($before.status -ne 'awaiting_approval') { throw 'Worker did not reach the gate.' }
docker compose restart server
if ($LASTEXITCODE -ne 0) { throw 'Server restart failed.' }
for ($i=0; $i -lt 30; $i++) {
    try { $after = (Invoke-RestMethod "$baseUrl/api/jobs/$($job.id)" -Headers $headers).job; break }
    catch { Start-Sleep -Seconds 1 }
}
if ($after.status -ne 'awaiting_approval' -or $after.gates[0].id -ne $before.gates[0].id -or $after.gates[0].artifact_digest -ne $before.gates[0].artifact_digest -or $after.attempts.Count -ne 1) { throw 'Gate state changed across restart.' }
Write-Host "PASS: server restarted; exact gate and artifact version survived. Job $($job.id) remains awaiting human review."
