param([string]$BaseUrl = 'http://127.0.0.1:8787')
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/env.ps1"
$identity = @($env:FACTORY_IDENTITIES | ConvertFrom-Json | Where-Object subject -eq 'local-maintainer')[0]
$headers = @{ Authorization = "Bearer $($identity.token)" }
function Request-Configuration($Path, $Body, $Method = 'POST') {
    Invoke-RestMethod "$BaseUrl/api/configuration$Path" -Method $Method -Headers $headers -ContentType 'application/json' -Body (ConvertTo-Json -InputObject $Body -Depth 30 -Compress)
}
function Start-ReleaseJob($Workflow) {
    $jobHeaders = @{Authorization=$headers.Authorization; 'Idempotency-Key'=[Guid]::NewGuid().ToString()}
    $body = @{workflow=$Workflow;repository='local-demo';issue=@{provider='fixture';key="RELEASE-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())";title='Release registry acceptance';body='Fixture-only prompt release validation.'}}
    $job = Invoke-RestMethod "$BaseUrl/api/jobs" -Method Post -Headers $jobHeaders -ContentType 'application/json' -Body ($body | ConvertTo-Json -Depth 8)
    $deadline = [DateTime]::UtcNow.AddMinutes(2)
    while ([DateTime]::UtcNow -lt $deadline) {
        $record = (Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)" -Headers $headers).job
        if ($record.status -eq 'succeeded') { return Invoke-RestMethod "$BaseUrl/api/jobs/$($job.id)/receipt" -Headers $headers }
        if ($record.status -in @('failed','cancelled','timed_out','rejected')) { throw "Release test job failed: $($record.status) $($record.attempts[-1].error)" }
        Start-Sleep -Seconds 1
    }
    throw 'Release test job timed out'
}
$root = "release-smoke-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())"
$prompt = "$root@1"
$source = @"
apiVersion: factory/v1
kind: Workflow
id: $root
version: 1
description: Fixture acceptance of portal-managed prompts
inputs:
  repository: {type: repository_ref}
  issue: {type: issue_ref}
defaults:
  workerProfile: fixture-standard
  agentProfile: fixture
  repositoryRevision: pin_at_job_start
phases:
  - id: research
    timeout: 5m
    permissions: []
    tasks:
      - uses: agent.execute
        with: {prompt: $prompt, output: research.md}
      - uses: artifact.publish
        with: {name: research, path: research.md}
"@
$bundle = @{format=1;workflow=$root;workflows=@{$root=$source};prompts=@{$prompt='Write the requested fixture research report.'};base_generation=0}
$draftId = [Guid]::NewGuid().ToString()
$draft = Request-Configuration "/drafts/$draftId" @{revision=0;bundle=$bundle} 'PUT'
if ($draft.revision -ne 1) { throw 'Draft not saved' }
$validation = Request-Configuration '/validate' $bundle
$first = Request-Configuration '/releases' @{bundle=$bundle;note='Acceptance original'}
$duplicate = Request-Configuration '/releases' @{bundle=$bundle;note='Acceptance duplicate'}
if ($first.id -ne $duplicate.id) { throw 'Import not idempotent' }
Request-Configuration "/releases/$($first.id)/activate" @{expected_generation=0;note='Acceptance activate'} | Out-Null
$original = Start-ReleaseJob $root
if ($original.release_id -ne $first.id) { throw 'Original job not pinned' }
$bundle.base_generation=1
$nextPrompt="$root@2"
$bundle.workflows[$root]=$source.Replace('version: 1','version: 2').Replace($prompt,$nextPrompt)
$bundle.prompts=@{$nextPrompt='Write the requested fixture research report with revised instructions.'}
$second = Request-Configuration '/releases' @{bundle=$bundle;note='Acceptance revised prompt'}
Request-Configuration "/releases/$($second.id)/activate" @{expected_generation=1;note='Acceptance update'} | Out-Null
$updated=Start-ReleaseJob $root
if ($updated.release_id -ne $second.id) { throw 'New job missed activated release' }
if ($original.prompts.$prompt.sha256 -eq $updated.prompts.$nextPrompt.sha256) { throw 'Updated prompt hash did not change' }
$taskArtifact = Invoke-WebRequest "$BaseUrl/api/artifacts/$($updated.attempts[0].artifacts.research.id)" -Headers $headers
if (-not ([Text.Encoding]::UTF8.GetString($taskArtifact.RawContentStream.ToArray())).Contains($updated.prompts.$nextPrompt.sha256)) { throw 'Worker did not execute the pinned updated prompt' }
$before = Invoke-RestMethod "$BaseUrl/api/jobs/$($original.job_id)/receipt" -Headers $headers
if ($before.release_id -ne $first.id) { throw 'Original receipt changed' }
Request-Configuration "/releases/$($first.id)/activate" @{expected_generation=2;note='Acceptance restore'} | Out-Null
$restored=Start-ReleaseJob $root
if ($restored.release_id -ne $first.id) { throw 'Restored release not used' }
$export = Invoke-RestMethod "$BaseUrl/api/configuration/releases/$($first.id)/export" -Headers $headers
if ($export.base_generation -ne 3 -or $export.prompts.$prompt -ne 'Write the requested fixture research report.') { throw 'Export does not preserve original prompt' }
Write-Output "PASS: draft, validation, idempotent publish, activation, changed prompt execution, immutable old receipt, rollback and export. Workflow $root"
