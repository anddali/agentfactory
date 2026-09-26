import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("validate", Path(__file__).with_name("validate.py"))
validate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validate)


class ValidationSelection(unittest.TestCase):
    def test_rust_uses_locked_cargo_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text("[package]")
            with self.assertRaisesRegex(RuntimeError, "Cargo.lock"):
                validate.commands(root)
            (root / "Cargo.lock").write_text("")
            self.assertEqual(validate.commands(root), [["cargo", "test", "--locked"]])

    def test_npm_installs_locked_dependencies_without_jest_specific_flags(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "package.json").write_text(json.dumps({"scripts": {"test": "node --test"}}))
            with self.assertRaisesRegex(RuntimeError, "lockfile"):
                validate.commands(root)
            (root / "package-lock.json").write_text("{}")
            self.assertEqual(validate.commands(root), [["npm", "ci"], ["npm", "test"]])
            (root / "package.json").write_text("{}")
            with self.assertRaisesRegex(RuntimeError, "no test script"):
                validate.commands(root)

    def test_unknown_project_does_not_report_success(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "No supported"):
                validate.commands(Path(directory))


if __name__ == "__main__":
    unittest.main()
