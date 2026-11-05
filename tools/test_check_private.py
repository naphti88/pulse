#!/usr/bin/env python3
"""The check of tools/check_private.py, in a throwaway repository: what must
stop a commit stops it, what is harmless passes. Run by `npm test` in desktop.
Every planted secret is put together here, so that this file passes the check."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHECK = ROOT / "tools/check_private.py"
HOOKS = ROOT / ".githooks"
ALLOWED = {"keys": ["lines", "type", "name"], "values": ["assistant", "Bash"], "templates": ["t{n}"]}


class Repo:
    def __init__(self, home):
        self.dir = Path(tempfile.mkdtemp())
        self.env = {**os.environ, "HOME": str(home), "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null"}
        self.git("init", "-q", "-b", "master")
        self.git("config", "user.name", "T")
        self.git("config", "user.email", "person" + "@mailhost.test")
        (self.dir / "tools").mkdir()
        shutil.copy(CHECK, self.dir / "tools/check_private.py")
        shutil.copytree(HOOKS, self.dir / ".githooks")
        self.git("config", "core.hooksPath", ".githooks")
        self.write("desktop/src-tauri/testdata/allowed.json", json.dumps(ALLOWED))
        self.write("README.md", "hello\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "start")

    def write(self, path, text):
        file = self.dir / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text, encoding="utf-8")

    def run(self, *args):
        return subprocess.run(args, cwd=self.dir, env=self.env, capture_output=True, text=True)

    def git(self, *args):
        done = self.run("git", *args)
        if done.returncode != 0:
            raise AssertionError(done.stderr)

    def commit(self, path, text):
        """Whether a commit of this file gets through the pre-commit hook."""
        self.write(path, text)
        self.git("add", "-f", path)
        return self.run("git", "commit", "-q", "-m", "x").returncode == 0

    def check(self, *args):
        return self.run("python3", "-I", "tools/check_private.py", *args)


class CheckPrivate(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp(prefix="pulsehome"))
        self.repo = Repo(self.home)
        self.addCleanup(shutil.rmtree, self.home, True)
        self.addCleanup(shutil.rmtree, self.repo.dir, True)

    def test_a_commit_with_something_private_is_stopped(self):
        planted = {
            "an Anthropic key": "key = " + "sk-" + "ant-" + "a" * 30,
            "a GitHub token": "token: " + "gh" + "p_" + "a" * 36,
            "a private key": "-----BEGIN " + "RSA PRIVATE KEY-----",
            "a bearer token": "Authorization: Bearer " + "a1" * 20,
            "a webhook": "https://discord.com/" + "api/webhooks/123/abc",
            "a mail address": "write to " + "someone@" + "mailhost.test",
            "the mail git signs with": "person" + "@mailhost.test",
            "a home folder": "/Users/" + "someone/Work/secret",
            "a session id": "local_" + "12345678-1234-1234-1234-123456789abc",
            "a transcript line": '{"parent' + 'Uuid":"x","type":"user"}',
        }
        for name, text in planted.items():
            with self.subTest(name):
                self.assertFalse(self.repo.commit("notes.txt", text + "\n"), name)
                self.repo.git("reset", "-q", "--hard")

    def test_files_that_are_private_by_what_they_are_are_stopped(self):
        for path in ["desktop/src-tauri/testdata/chat.jsonl", "captures/case.json", ".env", "state.json", "id.pem"]:
            with self.subTest(path):
                self.assertFalse(self.repo.commit(path, "x\n"), path)
                self.repo.git("reset", "-q", "--hard")

    def test_the_output_never_shows_what_it_found(self):
        secret = "sk-" + "ant-" + "s3cret" * 6
        self.repo.write("notes.txt", secret + "\n")
        self.repo.git("add", "notes.txt")
        done = self.repo.check("--staged")
        self.assertEqual(done.returncode, 1)
        self.assertNotIn("s3cret", done.stderr + done.stdout)

    def test_a_test_case_holds_only_what_is_listed(self):
        path = "desktop/src-tauri/testdata/case.json"
        good = {"lines": [{"type": "assistant", "name": "Bash"}, {"name": "t12"}]}
        self.assertTrue(self.repo.commit(path, json.dumps(good)))
        for bad in ({"lines": [{"type": "Kundenname Müller"}]}, {"secret": "assistant"}, {"lines": [{"name": "Bash and more"}]}):
            with self.subTest(bad):
                self.assertFalse(self.repo.commit(path, json.dumps(bad)))
                self.repo.git("reset", "-q", "--hard")
        self.assertFalse(self.repo.commit("desktop/src-tauri/testdata/notes.txt", "hello\n"))

    def test_the_words_of_the_private_list_are_stopped_and_only_as_words(self):
        (self.home / ".config/pulse").mkdir(parents=True)
        (self.home / ".config/pulse/private-words.txt").write_text("# customers\nAcmeCorp\n", encoding="utf-8")
        self.assertFalse(self.repo.commit("notes.txt", "for acmecorp only\n"))
        self.repo.git("reset", "-q", "--hard")
        self.assertTrue(self.repo.commit("notes.txt", "xacmecorpx\n"))

    def test_what_is_harmless_passes(self):
        text = "see icons/128x128@2x.png, noreply@anthropic.com, /Users/x/Work/shop-api, @types/node\n"
        self.assertTrue(self.repo.commit("notes.txt", text))

    def test_the_history_finds_what_went_around_the_hook(self):
        self.repo.write("notes.txt", "gh" + "p_" + "b" * 36 + "\n")
        self.repo.git("add", "notes.txt")
        self.repo.git("commit", "-q", "--no-verify", "-m", "around")
        self.repo.git("rm", "-q", "notes.txt")
        self.repo.git("commit", "-q", "-m", "gone")
        self.assertEqual(self.repo.check("--all").returncode, 0)
        self.assertEqual(self.repo.check("--history").returncode, 1)
        self.assertNotEqual(self.repo.run("sh", ".githooks/pre-push").returncode, 0)

    def test_a_check_that_cannot_run_stops_the_commit(self):
        (self.repo.dir / "tools/check_private.py").unlink()
        self.repo.write("notes.txt", "hello\n")
        self.repo.git("add", "notes.txt")
        self.assertNotEqual(self.repo.run("git", "commit", "-q", "-m", "x").returncode, 0)


if __name__ == "__main__":
    sys.exit(unittest.main(verbosity=1, exit=False).result.wasSuccessful() is False)
