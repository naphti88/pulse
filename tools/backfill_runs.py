"""Rebuild routine-run records from older Claude Code transcripts.

The routine-recorder plugin writes one JSON file per routine run into
~/.claude/routine-recorder/runs/<cliSessionId>.json while the run happens.
This script writes the same records for runs that happened before the plugin
was installed, from the transcripts in ~/.claude/projects/*/<id>.jsonl.

A record the plugin wrote (source "live" or "backfill+live") is never touched;
a record this script wrote earlier (source "backfill") is rebuilt.

Usage: python3 -I tools/backfill_runs.py [--dry-run]
"""

import json
import re
import sys
from datetime import datetime
from pathlib import Path

HOME = Path.home()
PROJECTS = HOME / ".claude" / "projects"
RUNS = HOME / ".claude" / "routine-recorder" / "runs"

TRIGGER = re.compile(r'^\s*<scheduled-task name="([^"]+)"(?: file="([^"]+)")?')
# the plain line as asked for, or as a markdown reference ("[//]: # (...)")
# or an HTML comment, as asked for for a while: each form with its own end,
# so a plain line keeps a final ")", and a line with nothing after the word
# takes nothing from the next line
STATUS_LINE = re.compile(
    r"^[ \t>*_`]*(?:<!--\s*STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*(?:[|:–]|-(?!->))?[^\S\r\n]*(.*?)\s*(?:-->)?"
    r"|\[//\]:\s*#\s*\(\s*STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*[|:–-]?[^\S\r\n]*(.*?)\s*\)?"
    r"|STATUS:\s*(idle|worked|waiting|blocked|parked)\b[^\S\r\n]*[|:–-]?[^\S\r\n]*(.*?))[\s*_`]*$",
    re.I | re.M,
)
PING_LINE = re.compile(
    r"^[ \t>*_`]*(?:<!--\s*PING:[^\S\r\n]*(\S.*?)\s*(?:-->)?|\[//\]:\s*#\s*\(\s*PING:[^\S\r\n]*(\S.*?)\s*\)?|PING:[^\S\r\n]*(\S.*?))[\s*_`]*$",
    re.I | re.M,
)
PING_TOOL = re.compile(r"^PushNotification$|^mcp__.+__(send|post|notify|reply|chat_post)", re.I)
WEBHOOK = re.compile(r"discord(?:app)?\.com/api/webhooks|hooks\.slack\.com", re.I)
UNANSWERED_TEXT = re.compile(r"went unanswered", re.I)
DENIED_TEXT = re.compile(r"doesn't want to proceed|permission.{0,40}denied|was denied|rejected", re.I)
INTERRUPTED = "[Request interrupted by user"
# the plugin's own self-test runs, not a routine of the person's
IGNORED_TASKS = {"recorder-selftest"}
# a command the person ran from the chat (the Play button, or "!") is a round of theirs;
# its output and local slash commands are not
NOT_A_PROMPT = ("<bash-stdout>", "<bash-stderr>", "<local-command-", "<command-name>")

MAX_TURNS = 200
MAX_PER_TURN = 50


def ms(timestamp):
    return int(datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp() * 1000)


def blocks(record):
    content = (record.get("message") or {}).get("content")
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    return [b for b in content or [] if isinstance(b, dict)]


def text_of(record):
    return "\n".join(b.get("text", "") for b in blocks(record) if b.get("type") == "text")


def result_text(block):
    content = block.get("content")
    if isinstance(content, list):
        return "\n".join(c.get("text", "") for c in content if isinstance(c, dict))
    return str(content or "")


def prompt_kind(record):
    """What starts a turn: (by, trigger) for a prompt, None for anything else."""
    if record.get("type") != "user" or record.get("isSidechain"):
        return None
    if any(b.get("type") == "tool_result" for b in blocks(record)):
        return None
    text = text_of(record).lstrip()
    if not text or text.startswith(NOT_A_PROMPT) or text.startswith(INTERRUPTED):
        return None
    origin = record.get("origin") or {}
    kind = origin.get("kind")
    if TRIGGER.match(text):
        scheduled = kind == "task-notification" or record.get("turnOrigin") == "scheduled"
        return ("routine", "scheduled" if scheduled else "manual")
    if kind in ("peer", "task-notification", "peer-send-message"):
        return ("system", None)
    if record.get("isMeta"):
        return None
    return ("user", None)


def new_turn(record_id, by, trigger, at):
    turn = {
        "id": record_id,
        "by": by,
        "startedAt": at,
        "pings": [],
        "tools": {},
        "toolErrors": 0,
        "permissions": [],
        "interjections": 0,
    }
    if trigger:
        turn["trigger"] = trigger
    return turn


def finish(turn, last_text, ended_at, end):
    if turn is None or ended_at is None:
        return
    turn["endedAt"] = ended_at
    turn["end"] = end
    if last_text:
        turn["answerTail"] = last_text.strip()[-600:]
        statuses = list(STATUS_LINE.finditer(last_text))
        if statuses:
            # the groups of the form that matched
            last = statuses[-1]
            turn["status"] = (last.group(1) or last.group(3) or last.group(5)).lower()
            text = (last.group(2) or last.group(4) or last.group(6) or "").strip()
            if text:
                turn["statusText"] = text
        for ping in PING_LINE.finditer(last_text):
            detail = ping.group(1) or ping.group(2) or ping.group(3)
            turn["pings"].append({"via": "PING line", "detail": detail[:200], "at": ended_at})


def rebuild(path):
    records = []
    for line in path.open(encoding="utf-8"):
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            continue

    first = next((r for r in records if prompt_kind(r) is not None), None)
    if first is None:
        return None
    trigger = TRIGGER.match(text_of(first).lstrip())
    if trigger is None or trigger.group(1) in IGNORED_TASKS:
        return None

    turns = []
    turn = None
    last_text = ""
    last_at = None
    end = "answer"
    calls = {}

    for record in records:
        if record.get("isSidechain") or "timestamp" not in record:
            continue
        at = ms(record["timestamp"])
        kind = prompt_kind(record)
        if kind is not None:
            finish(turn, last_text, last_at, end)
            turn = new_turn(record.get("uuid", str(at)), kind[0], kind[1], at)
            turns.append(turn)
            # a round with no answer (a command run, nothing said after) ends where it began
            last_text, last_at, end = "", at, "answer"
            continue
        if turn is None:
            continue

        if record.get("type") == "system" and record.get("subtype") == "api_error":
            end = "error"
        if record.get("type") == "user" and text_of(record).lstrip().startswith(INTERRUPTED):
            end = "aborted"

        if record.get("type") == "assistant":
            last_at = at
            for block in blocks(record):
                if block.get("type") == "text" and block.get("text", "").strip():
                    last_text = block["text"]
                if block.get("type") != "tool_use":
                    continue
                name = block.get("name", "?")
                tool_input = block.get("input") or {}
                calls[block.get("id")] = name
                turn["tools"][name] = turn["tools"].get(name, 0) + 1
                # a webhook posted from a shell: Bash, or PowerShell on Windows
                command = tool_input.get("command", "") if name in ("Bash", "PowerShell") else ""
                # a file sent to reach the person's phone pings them, as PushNotification does
                proactive = name == "SendUserFile" and tool_input.get("status") == "proactive"
                if PING_TOOL.search(name) or proactive or (command and WEBHOOK.search(command)):
                    detail = json.dumps({"tool": name, **tool_input}, ensure_ascii=False)[:200]
                    turn["pings"].append({"via": name, "detail": detail, "at": at})

        if record.get("type") == "user":
            decision = record.get("permissionDecision") or {}
            for block in blocks(record):
                if block.get("type") != "tool_result":
                    continue
                tool = calls.get(block.get("tool_use_id"), "?")
                text = result_text(block)
                is_error = bool(block.get("is_error"))
                if is_error:
                    turn["toolErrors"] += 1
                    turn["lastError"] = f"{tool}: {text[:200]}"
                by_person = decision.get("source") in ("user_temporary", "user_permanent")
                rejected = decision.get("decision") == "reject"
                unanswered = is_error and UNANSWERED_TEXT.search(text)
                denied = rejected or (is_error and DENIED_TEXT.search(text))
                if not (by_person or rejected or unanswered or denied):
                    continue
                entry = {
                    "tool": tool,
                    "toolUseId": block.get("tool_use_id"),
                    "decision": "deny" if decision.get("reasonType") == "rule" else "ask",
                    "at": at,
                    "outcome": "unanswered" if unanswered else "denied" if denied else "allowed",
                }
                reason = decision.get("reasonType") or decision.get("source")
                if by_person and not denied:
                    reason = "approved by you"
                if reason:
                    entry["reason"] = reason
                if is_error:
                    entry["outcomeText"] = text[:200]
                turn["permissions"].append(entry)

    finish(turn, last_text, last_at, end)
    turns = [t for t in turns if "endedAt" in t or t["tools"]][-MAX_TURNS:]
    for t in turns:
        t["pings"] = t["pings"][-MAX_PER_TURN:]
        t["permissions"] = t["permissions"][-MAX_PER_TURN:]
    if not turns:
        return None

    cwd = next((r.get("cwd") for r in records if r.get("cwd")), "")
    return {
        "version": 1,
        "source": "backfill",
        "sessionId": path.stem,
        "taskId": trigger.group(1),
        **({"taskFile": trigger.group(2)} if trigger.group(2) else {}),
        "cwd": cwd,
        "startedAt": turns[0]["startedAt"],
        "updatedAt": max(t.get("endedAt", t["startedAt"]) for t in turns),
        "activeTurnId": None,
        "turns": turns,
    }


def main():
    # piped or redirected on Windows, stdout writes the ANSI code page: a task
    # name's character it lacks prints as "?" instead of stopping the run
    # (pythonw's stdout is None, and IDLE's has no reconfigure)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    dry_run = "--dry-run" in sys.argv
    # a dry run writes nothing, not even the folder
    if not dry_run:
        RUNS.mkdir(parents=True, exist_ok=True)
    written = kept = skipped = 0
    for path in sorted(PROJECTS.glob("*/*.jsonl")):
        if "<scheduled-task name=" not in path.read_text(encoding="utf-8", errors="replace"):
            continue
        target = RUNS / f"{path.stem}.json"
        if target.exists():
            existing = json.loads(target.read_text(encoding="utf-8"))
            if existing.get("source") != "backfill":
                kept += 1
                continue
        record = rebuild(path)
        if record is None:
            skipped += 1
            continue
        if not dry_run:
            target.write_text(json.dumps(record, ensure_ascii=False, indent=1), encoding="utf-8")
        written += 1
        print(f"{record['taskId']:45} {len(record['turns']):3} turns  {path.stem}")
    print(f"\nwritten {written}, kept (recorded live) {kept}, not a routine run {skipped}"
          + (" [dry run]" if dry_run else ""))


if __name__ == "__main__":
    main()
