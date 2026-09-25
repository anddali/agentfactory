"""Bounded live check. Credential arrives over stdin, never a Docker env override."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import yaml

config = yaml.safe_load(Path("/config/platform.yaml").read_text())["agents"]["coding-default"]["openhands"].copy()
credential = json.load(sys.stdin)["credential"]
config["max_iterations"] = min(config["max_iterations"], 4)
config["max_output_tokens"] = min(config["max_output_tokens"], 2048)
config["timeout_seconds"] = min(config["timeout_seconds"], 120)
with tempfile.TemporaryDirectory() as workspace:
    env = dict(os.environ, HOME=workspace)
    env[config["api_key_env"]] = credential
    request = dict(config=config, output="provider-check.txt", review=False,
        prompt="Use a tool to create provider-check.txt in the current directory containing exactly FACTORY_OPENAI_OK. Verify its contents using a tool, then finish. This is a tiny integration check; do not inspect other files.")
    result = subprocess.run(["python", "/opt/factory/openhands_adapter.py"],
        input=json.dumps(request), text=True, capture_output=True, cwd=workspace, env=env,
        timeout=config["timeout_seconds"] + 15)
    if result.returncode:
        print(json.dumps({"status":"adapter_process_failed", "exit_code":result.returncode}))
        sys.exit(1)
    report = json.loads(result.stdout)
    output = Path(workspace, "provider-check.txt")
    report["output_verified"] = output.is_file() and output.read_text().strip() == "FACTORY_OPENAI_OK"
    print(json.dumps(report))
    sys.exit(0 if report["status"] == "finished" and report["output_verified"] else 1)
