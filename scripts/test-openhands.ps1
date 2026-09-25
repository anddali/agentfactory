$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges `
    --tmpfs /work:rw,uid=10001,gid=10001,size=536870912 --tmpfs /tmp:rw,size=134217728 `
    --mount "type=bind,source=$repoPath/harnesses/openhands,target=/tests,readonly" `
    --entrypoint python factory-worker:openhands /tests/test_adapter.py
if ($LASTEXITCODE -ne 0) { throw 'OpenHands adapter integration tests failed.' }
