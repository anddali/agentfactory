"""Exercise the installed SDK, HTTP transport and real shell tool without paid APIs."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import hashlib
from datetime import datetime, timedelta, timezone
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
import uuid


class Provider(BaseHTTPRequestHandler):
    mode = "success"
    calls = 0
    manifest = None
    completion = None
    artifact_bytes = None

    def reply(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_PUT(self):
        data = self.rfile.read(int(self.headers["Content-Length"]))
        type(self).artifact_bytes = data
        self.reply(dict(id=str(uuid.uuid4()), attempt_id=self.manifest["attempt_id"], name="result",
                        sha256=hashlib.sha256(data).hexdigest(), size=len(data), created_at=datetime.now(timezone.utc).isoformat()))

    def log_message(self, *_):
        pass

    def do_POST(self):
        if self.path.startswith("/worker/"):
            data = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            if self.path.endswith("/claim"):
                self.reply(self.manifest)
            else:
                if self.path.endswith("/complete"):
                    type(self).completion = json.loads(data)
                self.reply({})
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert "reasoning_effort" not in body, "Unrequested reasoning options break non-thinking models"
        type(self).calls += 1
        if self.mode == "slow":
            time.sleep(4)
        if self.mode == "error":
            self.send_response(401)
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"secret-test-key"}}')
            return
        terminal = self.calls == 1 and self.mode != "missing"
        name = "terminal" if terminal else "finish"
        assert name in [tool["function"]["name"] for tool in body["tools"]]
        args = {"command": "printf '# Adapter integration test\\n' > result.md"} if terminal else {"message": "Done"}
        response = dict(id="chatcmpl-test", object="chat.completion", created=1, model="gpt-4.1",
                        choices=[dict(index=0, finish_reason="tool_calls", message=dict(role="assistant", content=None,
                          tool_calls=[dict(id=f"call_{self.calls}", type="function", function=dict(name=name, arguments=json.dumps(args)))]))],
                        usage=dict(prompt_tokens=10, completion_tokens=5, total_tokens=15))
        data = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except BrokenPipeError:
            pass


class AdapterTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def execute(self, mode="success", **overrides):
        Provider.mode, Provider.calls = mode, 0
        config = dict(sdk_version="1.49.2", model="openai/gpt-4.1", api_key_env="TEST_MODEL_KEY",
                      base_url=f"http://127.0.0.1:{self.server.server_port}/v1", max_iterations=4,
                      api_mode="chat", max_output_tokens=128, timeout_seconds=30, tools=["terminal", "file_editor"])
        config.update(overrides)
        with tempfile.TemporaryDirectory() as workspace:
            env = dict(os.environ, HOME=workspace, TEST_MODEL_KEY="secret-test-key", OPENHANDS_SUPPRESS_BANNER="1")
            result = subprocess.run(["python", "/opt/factory/openhands_adapter.py"],
                input=json.dumps(dict(config=config, output="result.md", prompt="Write result.md", review=False)),
                cwd=workspace, env=env, text=True, capture_output=True, timeout=45)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("secret-test-key", result.stdout + result.stderr)
            receipt = json.loads(result.stdout)
            exists = Path(workspace, "result.md").is_file()
        return receipt, exists

    def test_real_sdk_executes_tool_and_reports_usage(self):
        receipt, exists = self.execute()
        self.assertTrue(exists)
        self.assertEqual(receipt["status"], "finished")
        self.assertEqual(receipt["model"], "openai/gpt-4.1")
        self.assertEqual(receipt["harness_version"], "1.49.2")
        self.assertGreater(receipt["prompt_tokens"], 0)

    def test_finish_without_output_is_failure(self):
        receipt, _ = self.execute("missing")
        self.assertEqual(receipt["status"], "invalid_output")

    def test_provider_error_does_not_leak_credentials(self):
        receipt, _ = self.execute("error")
        self.assertNotEqual(receipt["status"], "finished")

    def test_iteration_limit_does_not_report_success(self):
        receipt, _ = self.execute(max_iterations=1)
        self.assertNotEqual(receipt["status"], "finished")

    def test_deadline_is_enforced(self):
        receipt, _ = self.execute("slow", timeout_seconds=1)
        self.assertNotEqual(receipt["status"], "finished")

    def test_version_mismatch_fails_before_calling_provider(self):
        receipt, _ = self.execute(sdk_version="wrong")
        self.assertEqual(receipt["status"], "error")
        self.assertEqual(Provider.calls, 0)

    def test_rust_worker_claims_runs_harness_publishes_and_reports(self):
        Provider.mode, Provider.calls, Provider.completion = "success", 0, None
        attempt = str(uuid.uuid4())
        now = datetime.now(timezone.utc)
        prompt = "Write result.md"
        Provider.manifest = dict(job_id=str(uuid.uuid4()), attempt_id=attempt,
            phase=dict(id="research", timeout="1m", tasks=[
                dict(uses="agent.execute", **{"with":dict(prompt="test@1", output="result.md")}),
                dict(uses="artifact.publish", **{"with":dict(name="result", path="result.md")})]),
            repository=dict(id="test", url="fixture://test", revision="0"*40, base_branch="main", work_branch="test", provider="fixture", api_url="fixture://test"),
            issue=dict(provider="fixture", key="TEST", title="Harness test", body="test"),
            prompts={"test@1":dict(text=prompt, sha256=hashlib.sha256(prompt.encode()).hexdigest())},
            agent=dict(version=1, backend="openhands", env_keys=["TEST_MODEL_KEY"], openhands=dict(
                sdk_version="1.49.2", model="openai/gpt-4.1", api_key_env="TEST_MODEL_KEY",
                base_url=f"http://127.0.0.1:{self.server.server_port}/v1", api_mode="chat",
                max_iterations=4, max_output_tokens=128, timeout_seconds=30, tools=["terminal", "file_editor"])),
            validations={}, inputs={}, deadline=(now+timedelta(seconds=60)).isoformat(), commit_time=now.isoformat(),
            credentials=dict(agent_env={"TEST_MODEL_KEY":"secret-test-key"}))
        env = dict(os.environ, FACTORY_SERVER_URL=f"http://127.0.0.1:{self.server.server_port}",
                   FACTORY_ATTEMPT_ID=attempt, FACTORY_ATTEMPT_TOKEN="test-attempt-token")
        worker = subprocess.run(["factory-worker"], env=env, capture_output=True, text=True, timeout=65)
        self.assertEqual(worker.returncode, 0, worker.stderr)
        self.assertIsNotNone(Provider.completion)
        self.assertTrue(Provider.completion["succeeded"], Provider.completion)
        self.assertEqual(Provider.completion["agent_runs"][0]["harness"], "openhands")
        self.assertEqual(len(Provider.completion["tasks"]), 2)
        self.assertIn(b"Adapter integration test", Provider.artifact_bytes)
        self.assertNotIn("secret-test-key", json.dumps(Provider.completion))


if __name__ == "__main__":
    unittest.main(verbosity=2)
