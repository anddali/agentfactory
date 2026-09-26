"""Trusted validation entry point; never guess runner-specific flags."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def commands(root):
    if (root / "Cargo.toml").is_file():
        if not (root / "Cargo.lock").is_file():
            raise RuntimeError("Rust validation requires Cargo.lock or a custom validation profile")
        return [["cargo", "test", "--locked"]]
    if (root / "package.json").is_file():
        package = json.loads((root / "package.json").read_text())
        if not package.get("scripts", {}).get("test"):
            raise RuntimeError("package.json has no test script; configure a validation profile")
        if not (root / "package-lock.json").is_file() and not (root / "npm-shrinkwrap.json").is_file():
            raise RuntimeError("npm validation requires a lockfile; configure a profile for your package manager")
        return [["npm", "ci"], ["npm", "test"]]
    raise RuntimeError("No supported root test project found; configure a repository validation profile")


def main():
    # Bound compilation within the standard 2 CPU / 4 GB worker profile.
    os.environ.setdefault("CARGO_BUILD_JOBS", "2")
    os.environ.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    os.environ.setdefault("CARGO_PROFILE_TEST_DEBUG", "0")
    for command in commands(Path.cwd()):
        if not shutil.which(command[0]):
            raise RuntimeError(f"Required executable {command[0]} is missing from the worker image")
        subprocess.run(command, check=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"Repository validation failed: {error}", file=sys.stderr)
        sys.exit(1)
