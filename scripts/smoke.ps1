param([string]$BaseUrl = 'http://127.0.0.1:8787', [switch]$LeavePending)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/env.ps1"
$identity = @($env:FACTORY_IDENTITIES | ConvertFrom-Json)[0]
$headers = @{ Authorization = "Bearer $($identity.token)"; 'Idempotency-Key' = "smoke-$([Guid]::NewGuid())" }
$request = @{workflow='demo';repository='local-demo';issue=@{provider='fixture';key="DEMO-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())";title='Verify the durable factory workflow';body='Fixture acceptance run through research, plan and delivery.'}} | ConvertTo-Json -Depth 5
$job = Invoke-RestMethod "$BaseUrl/api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $request
$duplicate = Invoke-RestMethod "$BaseUrl/api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $request
if ($job.id -ne $duplicate.id) { throw 'Duplicate request created another job.' }
Write-Host "Created $($job.id); duplicate request correctly returned the same job."
$deadline = [DateTime]::UtcNow.AddMinutes(3)
$approved = @{}
while ([DateTime]::UtcNow -lt $deadline) {
    $record = (Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)" -Headers $headers).job
    if ($record.status -eq 'awaiting_approval') {
        $gate = @($record.gates | Where-Object status -eq 'pending')[0]
        if ($LeavePending) { Write-Host "Job is durably waiting at $($gate.phase). Gate $($gate.id)."; exit 0 }
        if (-not $approved.ContainsKey($gate.id)) {
            $decision = @{event_id="smoke-approve-$($gate.id)";gate_id=$gate.id;artifact_digest=$gate.artifact_digest;approve=$true} | ConvertTo-Json
            Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)/decisions" -Method Post -Headers $headers -ContentType 'application/json' -Body $decision | Out-Null
            Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)/decisions" -Method Post -Headers $headers -ContentType 'application/json' -Body $decision | Out-Null
            $approved[$gate.id] = $true
            Write-Host "Approved exact $($gate.phase) artifact; replay was idempotent."
        }
    } elseif ($record.status -eq 'succeeded') {
        $receipt = Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)/receipt" -Headers $headers
        if ($receipt.attempts.Count -ne 3 -or $receipt.gates.Count -ne 2) { throw 'Unexpected receipt shape.' }
        $artifact = $receipt.attempts[2].artifacts.delivery
        $download = Invoke-WebRequest "$BaseUrl/api/artifacts/$($artifact.id)" -Headers $headers
        $bytes = $download.RawContentStream.ToArray()
        $actual = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
        if ($actual -ne $artifact.sha256) { throw 'Downloaded artifact failed integrity verification.' }
        Write-Host "PASS: 3 fresh phase attempts, 2 approvals, durable receipt and verified artifact. Job $($job.id)"
        exit 0
    } elseif ($record.status -in @('failed','timed_out','rejected','cancelled')) { throw "Job ended as $($record.status): $($record.attempts[-1].error)" }
    Start-Sleep -Seconds 2
}
throw 'Smoke test exceeded its deadline.'
