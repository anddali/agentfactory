"""Factories subprocess contract: one JSON request in, one safe execution receipt out."""
import contextlib
import importlib.metadata
import json
import logging
import os
from pathlib import Path
import signal
import sys


def contained_file(name):
    root = Path.cwd().resolve()
    path = (root / name).resolve()
    if not name or Path(name).is_absolute() or not path.is_relative_to(root) or path == root:
        raise ValueError("output must remain in workspace")
    return path


def run(request):
    config = request["config"]
    version = importlib.metadata.version("openhands-sdk")
    report = dict(harness="openhands", harness_version=version, model=config["model"],
                  status="error", prompt_tokens=0, completion_tokens=0)
    conversation = None
    def deadline(_signal, _frame):
        raise TimeoutError("harness deadline")
    signal.signal(signal.SIGALRM, deadline)
    signal.alarm(config["timeout_seconds"])
    try:
        if version != config["sdk_version"]:
            raise ValueError("SDK version mismatch")
        output = contained_file(request["output"])
        key = os.environ.get(config["api_key_env"])
        if not key:
            raise ValueError("provider credential missing")
        from openhands.sdk import LLM, Agent, Conversation, Tool
        from openhands.tools.file_editor import FileEditorTool
        from openhands.tools.terminal import TerminalTool
        classes = {"terminal": TerminalTool, "file_editor": FileEditorTool}
        llm = LLM(model=config["model"], api_key=key, base_url=config.get("base_url"),
                  api_mode=config.get("api_mode", "auto"),
                  reasoning_effort=config.get("reasoning_effort"),
                  max_output_tokens=config["max_output_tokens"], num_retries=0,
                  timeout=min(120, config["timeout_seconds"]), usage_id="factory-agent")
        agent = Agent(llm=llm, tools=[Tool(name=classes[t].name) for t in config["tools"]])
        conversation = Conversation(agent=agent, workspace=str(Path.cwd()),
                                    max_iteration_per_run=config["max_iterations"],
                                    visualizer=None)
        prompt = request["prompt"] + "\nWrite the requested output file before finishing. Repository publishing and human approvals are handled by Factories."
        if request.get("review"):
            prompt += '\nAlso write review.json containing an object with a findings array.'
        conversation.send_message(prompt)
        conversation.run()
        report["status"] = conversation.state.execution_status.value
        if report["status"] == "finished":
            output = contained_file(request["output"])
            if not output.is_file() or output.stat().st_size > 10 * 1024 * 1024:
                report["status"] = "invalid_output"
            elif request.get("review"):
                review = json.loads(contained_file("review.json").read_text())
                if not isinstance(review, dict) or not isinstance(review.get("findings"), list):
                    report["status"] = "invalid_output"
    except TimeoutError:
        report["status"] = "timed_out"
    except Exception:
        # Provider responses and SDK exceptions can contain credentials or repository
        # content. Only stable status and numeric usage cross this boundary.
        report["status"] = "error"
    finally:
        signal.alarm(0)
        if conversation is not None:
            usage = conversation.state.stats.get_combined_metrics().get_snapshot().accumulated_token_usage
            if usage is not None:
                report["prompt_tokens"] = usage.prompt_tokens
                report["completion_tokens"] = usage.completion_tokens
            conversation.close()
    return report


def main():
    request = json.load(sys.stdin)
    logging.disable(logging.CRITICAL)
    # Keep the protocol clean and avoid persisting provider payloads in task logs.
    with open(os.devnull, "w") as sink, contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        report = run(request)
    print(json.dumps(report))


if __name__ == "__main__":
    main()
