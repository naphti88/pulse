#!/usr/bin/env python3
"""Keeps what is private out of git: chats, transcripts, paths, keys.

The repository is public and what was pushed once cannot be called back, so a
commit is stopped before it exists. Three places run this check: the hooks in
.githooks (a commit, a push), and CI for what got past them.

  --staged     what is about to be committed (the pre-commit hook)
  --all        every tracked file as it is now
  --history    every line ever added, in every commit (the pre-push hook, CI)

A hit names the file, the line and the rule, never the text it found: the
output of CI is public too. There is no marker to wave a line through. A false
hit is fixed by changing the line, or a list below, in a reviewed commit.

Words only you know (a customer, a project) belong in
~/.config/pulse/private-words.txt, one per line, # for a comment. That file
stays on the Mac: a list of what is private must not be public itself.

Standard library only. Exit code 0: clean, 1: something was found, 2: it could
not be checked, which stops a commit like a hit does.
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

TESTDATA = "desktop/src-tauri/testdata/"
ALLOWED = TESTDATA + "allowed.json"
PRIVATE_WORDS = Path.home() / ".config/pulse/private-words.txt"

# files nobody needs to read to judge, and that are full of hashes and addresses
SKIPPED = re.compile(r"(^|/)(Cargo\.lock|package-lock\.json)$")

FORBIDDEN_PATHS = [
    (re.compile(r"\.jsonl$"), "a transcript file"),
    (re.compile(r"(^|/)captures/"), "a capture folder"),
    (re.compile(r"(^|/)routine-recorder/runs/"), "a routine record"),
    (re.compile(r"(^|/)(state|session-history)\.json$"), "app state"),
    (re.compile(r"(^|/)\.env(\.|$)"), "an env file"),
    (re.compile(r"\.(pem|p12|key)$"), "a key file"),
]

EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,})")
EMAIL_DOMAINS = {"example.com", "example.org", "example.net", "anthropic.com", "users.noreply.github.com", "github.com"}
# icon@2x.png is a picture, not a mailbox
PICTURE = re.compile(r"\.(png|jpe?g|gif|svg|webp|ico|icns)$", re.IGNORECASE)

HOME_PATH = re.compile(r"(?:/Users|/home)/([A-Za-z0-9._-]+)")
HOME_NAMES = {"x", "t", "me", "you", "name", "user", "example", "runner"}

TEXT_RULES = [
    (re.compile(r"sk-ant-[A-Za-z0-9_-]{20,}"), "an Anthropic key"),
    (re.compile(r"sk-[A-Za-z0-9]{32,}"), "an API key"),
    (re.compile(r"\bgh[pousr]_[A-Za-z0-9]{30,}"), "a GitHub token"),
    (re.compile(r"\bgithub_pat_[A-Za-z0-9_]{30,}"), "a GitHub token"),
    (re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{20,}"), "a Slack token"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "an AWS key"),
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"), "a private key"),
    (re.compile(r"\bBearer [A-Za-z0-9._~+/-]{24,}"), "a bearer token"),
    (re.compile(r"discord(?:app)?\.com/api/webhooks/[0-9]+/"), "a webhook address"),
    (re.compile(r"hooks\.slack\.com/services/T"), "a webhook address"),
    (re.compile(r"\blocal_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"), "a session id of the app"),
    # fields only a real transcript has, as JSON, quotes included
    (re.compile(r'"(parentUuid|toolUseResult|promptId|requestId|leafUuid|sourceToolAssistantUUID)"\s*:'), "a transcript line"),
]


def git(*args, stdin=None):
    done = subprocess.run(["git", *args], capture_output=True, input=stdin)
    if done.returncode != 0:
        raise OSError(f"git {' '.join(args)}: {done.stderr.decode(errors='replace').strip()}")
    return done.stdout


def personal_rules():
    """What only this Mac knows: its home, the mail git signs with, the words of the list."""
    rules = []
    home = str(Path.home())
    if home.count("/") >= 2 and home not in ("/root",) and Path(home).name not in HOME_NAMES:
        rules.append((re.compile(re.escape(home)), "this Mac's home folder"))
    try:
        mail = git("config", "user.email").decode().strip()
    except OSError:
        mail = ""
    if mail and not mail.endswith("users.noreply.github.com"):
        rules.append((re.compile(re.escape(mail), re.IGNORECASE), "your mail address"))
    if PRIVATE_WORDS.exists():
        for word in PRIVATE_WORDS.read_text(encoding="utf-8").splitlines():
            word = word.split("#")[0].strip()
            if word:
                rules.append((re.compile(r"(?<![A-Za-z0-9])" + re.escape(word) + r"(?![A-Za-z0-9])", re.IGNORECASE), "a word of your private list"))
    return rules


def line_hits(line, rules):
    hits = [name for pattern, name in rules if pattern.search(line)]
    hits += ["a mail address" for m in EMAIL.finditer(line) if m.group(1).lower() not in EMAIL_DOMAINS and not PICTURE.search(m.group(1))]
    hits += ["a home folder" for m in HOME_PATH.finditer(line) if m.group(1) not in HOME_NAMES]
    return hits


def check_path(path):
    return [name for pattern, name in FORBIDDEN_PATHS if pattern.search(path)]


def is_allowed_value(text, allowed):
    """A value of the list, or a template of it, where {n} stands for digits."""
    if text in allowed["values"]:
        return True
    return any(re.fullmatch(re.escape(t).replace(r"\{n\}", "[0-9]+"), text) for t in allowed["templates"])


def case_problems(value, allowed, where="$"):
    """A test case holds only known keys and known values: what is not listed is refused."""
    if isinstance(value, dict):
        for key, inner in value.items():
            if key not in allowed["keys"]:
                yield f"unknown key at {where}"
            else:
                yield from case_problems(inner, allowed, f"{where}.{key}")
    elif isinstance(value, list):
        for i, inner in enumerate(value):
            yield from case_problems(inner, allowed, f"{where}[{i}]")
    elif isinstance(value, str) and not is_allowed_value(value, allowed):
        yield f"a value not on the list at {where}"


def check_case(path, text, allowed):
    if not path.endswith(".json"):
        return ["testdata holds only .json"]
    try:
        return list(case_problems(json.loads(text), allowed))
    except ValueError:
        return ["testdata that is not JSON"]


def check_file(path, data, rules, allowed):
    found = [(0, name) for name in check_path(path)]
    if SKIPPED.search(path) or b"\0" in data[:8192]:
        return found
    text = data.decode("utf-8", errors="replace")
    for number, line in enumerate(text.split("\n"), 1):
        found += [(number, name) for name in line_hits(line, rules)]
    if path.startswith(TESTDATA) and path != ALLOWED:
        found += [(0, name) for name in check_case(path, text, allowed)]
    return found


def load_allowed(read):
    try:
        return json.loads(read(ALLOWED))
    except (OSError, ValueError):
        return {"keys": [], "values": [], "templates": []}


def staged(rules):
    allowed = load_allowed(lambda p: git("show", f":{p}"))
    names = git("diff", "--cached", "--name-only", "--diff-filter=ACMRT", "-z").decode().split("\0")
    for path in filter(None, names):
        for number, name in check_file(path, git("show", f":{path}"), rules, allowed):
            yield path, number, name


def tracked(rules):
    allowed = load_allowed(lambda p: Path(p).read_bytes())
    for path in filter(None, git("ls-files", "-z").decode().split("\0")):
        if Path(path).is_file():
            for number, name in check_file(path, Path(path).read_bytes(), rules, allowed):
                yield path, number, name


HUNK = re.compile(r"^@@ -\S+ \+(\d+)")


def history(rules):
    """Every line a commit added, in every commit, also those no branch shows now."""
    for commit in git("rev-list", "--all").decode().split():
        short = commit[:9]
        path, number = "", 0
        diff = git("show", "--format=", "--unified=0", "--no-color", "--no-ext-diff", commit).decode("utf-8", errors="replace")
        for line in diff.split("\n"):
            if line.startswith("+++ "):
                path = line[6:] if line.startswith("+++ b/") else ""
                for name in check_path(path):
                    yield f"{short}:{path}", 0, name
            elif line.startswith("@@"):
                hunk = HUNK.match(line)
                number = int(hunk.group(1)) if hunk else 0
            elif line.startswith("+") and path and not SKIPPED.search(path):
                for name in line_hits(line[1:], rules):
                    yield f"{short}:{path}", number, name
                number += 1


def main():
    parser = argparse.ArgumentParser(description="Keeps what is private out of git.")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--staged", action="store_true")
    mode.add_argument("--all", action="store_true")
    mode.add_argument("--history", action="store_true")
    args = parser.parse_args()
    try:
        os.chdir(git("rev-parse", "--show-toplevel").decode().strip())
        rules = TEXT_RULES + personal_rules()
        check = staged if args.staged else tracked if args.all else history
        found = sorted(set(check(rules)))
    except (OSError, ValueError) as error:
        print(f"check_private: could not check: {error}", file=sys.stderr)
        return 2
    for path, number, name in found:
        print(f"{path}:{number}: {name}", file=sys.stderr)
    if found:
        print(f"check_private: {len(found)} hit(s). Nothing private may enter git; see tools/check_private.py.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
