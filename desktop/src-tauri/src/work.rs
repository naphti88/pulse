// Work that has not landed: a session says it is finished, but what it changed
// was never committed, pushed or put into a PR. Git and the app's own records
// tell, on this Mac: Pulse never fetches, commits or pushes anything.
//
// A folder is often shared by several sessions, and most of what is uncommitted
// there is not theirs. So a session answers for the files it edited itself, and
// a session in a worktree of its own for all of it. A command that changed files
// through the shell leaves no name in the transcript: such a file counts for the
// session if it last changed while a shell call of that session ran. What was
// deleted that way leaves nothing to look at and is not seen, except in a
// worktree. Branches come from what the app noted a session wrote to; a PR
// counts for a branch whichever session opened it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::inputs::Session;
use crate::rules::limits::{GIT_FRESH_MS, WORK_WINDOW_MS};
use crate::rules::SHELL;
use crate::util::{mtime_ms, parse_time, read_tail};

const EDITS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("/usr/bin/git").args(["--no-optional-locks", "-C", dir]).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

// the paths of `git status --porcelain -z`; a rename is followed by its old name
fn parse_status(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = text.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let Some((code, path)) = entry.split_at_checked(3) else { continue };
        paths.push(path.to_string());
        if code.starts_with(['R', 'C']) {
            entries.next();
        }
    }
    paths
}

#[derive(Clone, Default)]
struct Repo {
    // absolute
    dirty: Vec<String>,
    default_branch: Option<String>,
    has_remote: bool,
}

static REPOS: LazyLock<Mutex<HashMap<String, (f64, Option<Repo>)>>> = LazyLock::new(Default::default);

fn read_repo(dir: &str) -> Option<Repo> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])?.trim().to_string();
    let status = git(dir, &["status", "--porcelain", "-z"])?;
    let dirty = parse_status(&status).into_iter().map(|p| format!("{root}/{p}")).collect();
    let has_remote = git(dir, &["remote"]).is_some_and(|r| r.lines().any(|l| l == "origin"));
    let head = git(dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    let default_branch = head.and_then(|h| h.trim().strip_prefix("origin/").map(str::to_string)).or_else(|| {
        ["main", "master"].into_iter().find(|b| git(dir, &["rev-parse", "--verify", "-q", &format!("refs/remotes/origin/{b}")]).is_some()).map(str::to_string)
    });
    Some(Repo { dirty, default_branch, has_remote })
}

fn repo_at(dir: &str, now: f64) -> Option<Repo> {
    if let Some((at, repo)) = REPOS.lock().expect("repos").get(dir) {
        if now - at < GIT_FRESH_MS {
            return repo.clone();
        }
    }
    let repo = read_repo(dir);
    REPOS.lock().expect("repos").insert(dir.to_string(), (now, repo.clone()));
    repo
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

// where a branch stands, for a branch no PR is known of
#[derive(Debug, PartialEq)]
struct Branch {
    // commits that are not on the default branch
    ahead: usize,
    // commits origin does not have; none: it was never pushed
    unpushed: Option<usize>,
}

fn branch_note(name: &str, is_default: bool, branch: &Branch) -> Option<String> {
    match branch.unpushed {
        Some(0) if is_default || branch.ahead == 0 => None,
        Some(0) => Some(format!("{name} has no PR")),
        Some(n) => Some(format!("{} on {name} not pushed", plural(n, "commit"))),
        None if branch.ahead == 0 => None,
        None => Some(format!("{} on {name} not pushed", plural(branch.ahead, "commit"))),
    }
}

fn count(dir: &str, range: &str) -> Option<usize> {
    git(dir, &["rev-list", "--count", range])?.trim().parse().ok()
}

fn read_branch(dir: &str, name: &str, default: &str) -> Option<Branch> {
    git(dir, &["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")])?;
    let base = format!("refs/remotes/origin/{default}");
    let local = format!("refs/heads/{name}");
    let ahead = count(dir, &format!("{base}..{local}"))?;
    let upstream = format!("refs/remotes/origin/{name}");
    let unpushed = git(dir, &["rev-parse", "--verify", "-q", &upstream]).and_then(|_| count(dir, &format!("{upstream}..{local}")));
    Some(Branch { ahead, unpushed })
}

static BRANCHES: LazyLock<Mutex<HashMap<(String, String), (f64, Option<String>)>>> = LazyLock::new(Default::default);

// a line on a branch of a repository, if there is anything to say
fn note_of(dir: &str, name: &str, repo: &Repo, now: f64) -> Option<String> {
    let default = repo.default_branch.as_deref()?;
    let key = (dir.to_string(), name.to_string());
    if let Some((at, note)) = BRANCHES.lock().expect("branches").get(&key) {
        if now - at < GIT_FRESH_MS {
            return note.clone();
        }
    }
    let note = read_branch(dir, name, default).and_then(|branch| branch_note(name, name == default, &branch));
    BRANCHES.lock().expect("branches").insert(key, (now, note.clone()));
    note
}

// what a transcript shows of the files a session changed
#[derive(Clone, Default)]
struct Seen {
    // how far it was read
    offset: u64,
    files: HashSet<String>,
    // when its shell calls ran
    shell: Vec<(f64, f64)>,
    open: HashMap<String, f64>,
}

impl Seen {
    fn add(&mut self, other: Seen) {
        self.files.extend(other.files);
        self.shell.extend(other.shell);
    }

    // it last changed while one of the shell calls ran
    fn changed_by_shell(&self, path: &str) -> bool {
        std::fs::metadata(path).is_ok_and(|meta| {
            let at = mtime_ms(&meta);
            self.shell.iter().any(|(start, end)| *start <= at && at <= *end)
        })
    }
}

static TOUCHED: LazyLock<Mutex<HashMap<PathBuf, Seen>>> = LazyLock::new(Default::default);

fn read_line(line: &str, seen: &mut Seen) {
    let is_call = line.contains("\"tool_use\"");
    if !is_call && (seen.open.is_empty() || !line.contains("\"tool_result\"")) {
        return;
    }
    let Ok(entry) = serde_json::from_str::<Value>(line) else { return };
    let at = entry.get("timestamp").and_then(Value::as_str).and_then(parse_time);
    let blocks = entry.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
    for block in blocks.into_iter().flatten() {
        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") if EDITS.contains(&name) => {
                if let Some(file) = block.get("input").and_then(|i| i.get("file_path")).and_then(Value::as_str) {
                    seen.files.insert(file.to_string());
                }
            }
            Some("tool_use") if SHELL.contains(&name) => {
                if let (Some(id), Some(at)) = (block.get("id").and_then(Value::as_str), at) {
                    seen.open.insert(id.to_string(), at);
                }
            }
            Some("tool_result") => {
                let id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                if let (Some(start), Some(end)) = (seen.open.remove(id), at) {
                    seen.shell.push((start, end));
                }
            }
            _ => {}
        }
    }
}

fn scan(path: &Path) -> Seen {
    let Ok(meta) = std::fs::metadata(path) else { return Seen::default() };
    let size = meta.len();
    let known = TOUCHED.lock().expect("touched").get(path).cloned();
    let mut seen = known.filter(|seen| seen.offset <= size).unwrap_or_default();
    if seen.offset == size {
        return seen;
    }
    let Ok(buffer) = read_tail(path, size, size - seen.offset) else { return seen };
    // only whole lines: one still being written is read next time
    let end = buffer.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    for line in String::from_utf8_lossy(&buffer[..end]).split('\n') {
        read_line(line, &mut seen);
    }
    seen.offset += end as u64;
    TOUCHED.lock().expect("touched").insert(path.to_path_buf(), seen.clone());
    seen
}

// what a session changed, its subagents included
fn touched(transcript: &Path) -> Seen {
    let mut seen = scan(transcript);
    let agents = transcript.with_extension("").join("subagents");
    if let Ok(entries) = std::fs::read_dir(agents) {
        for path in entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl")) {
            seen.add(scan(&path));
        }
    }
    seen
}

fn is_worktree(dir: &str) -> bool {
    dir.contains("/.claude/worktrees/")
}

fn notes(s: &Session, transcript: Option<&PathBuf>, with_pr: &HashSet<&str>, now: f64) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = s.cwd.as_deref().filter(|d| Path::new(d).is_dir()) {
        if let Some(repo) = repo_at(dir, now) {
            let files = if is_worktree(dir) {
                repo.dirty.len()
            } else if repo.dirty.is_empty() {
                0
            } else {
                let seen = transcript.map(|t| touched(t)).unwrap_or_default();
                repo.dirty.iter().filter(|p| seen.files.contains(*p) || seen.changed_by_shell(p)).count()
            };
            if files > 0 {
                out.push(format!("{} not committed", plural(files, "file")));
            }
        }
    }
    for (dir, name) in &s.written {
        if with_pr.contains(name.as_str()) || !Path::new(dir).is_dir() {
            continue;
        }
        let Some(repo) = repo_at(dir, now).filter(|r| r.has_remote) else { continue };
        out.extend(note_of(dir, name, &repo, now));
    }
    out
}

// by session; only recent ones that do not run: the rest is history
pub fn left(sessions: &[Session], transcripts: &HashMap<String, PathBuf>, running: &HashSet<String>, now: f64) -> HashMap<String, Vec<String>> {
    let with_pr: HashSet<&str> = sessions.iter().flat_map(|s| s.prs.iter().map(String::as_str)).collect();
    sessions
        .iter()
        .filter(|s| !s.archived && !running.contains(&s.id) && s.last_activity.unwrap_or(0.0) > now - WORK_WINDOW_MS)
        .filter_map(|s| {
            let notes = notes(s, transcripts.get(&s.id), &with_pr, now);
            (!notes.is_empty()).then(|| (s.id.clone(), notes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_git_says_is_uncommitted() {
        assert_eq!(parse_status(" M a.rs\0?? b c.rs\0R  new.rs\0old.rs\0"), vec!["a.rs", "b c.rs", "new.rs"]);
        assert!(parse_status("").is_empty());
    }

    #[test]
    fn a_branch_says_how_far_it_came() {
        let note = |ahead, unpushed, default| branch_note("x", default, &Branch { ahead, unpushed });
        assert_eq!(note(2, None, false).as_deref(), Some("2 commits on x not pushed"));
        assert_eq!(note(3, Some(1), false).as_deref(), Some("1 commit on x not pushed"));
        assert_eq!(note(2, Some(0), false).as_deref(), Some("x has no PR"));
        // nothing beyond the default branch: merged, or never used
        assert_eq!(note(0, Some(0), false), None);
        assert_eq!(note(0, None, false), None);
        // the default branch needs no PR, only a push
        assert_eq!(note(1, Some(0), true), None);
        assert_eq!(note(0, Some(4), true).as_deref(), Some("4 commits on x not pushed"));
    }

    #[test]
    fn only_edits_count_as_touched_and_by_their_path() {
        let mut seen = Seen::default();
        let call = |name: &str, path: &str| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": name, "input": {"file_path": path}}]}}).to_string();
        read_line(&call("Edit", "/w/a.rs"), &mut seen);
        read_line(&call("Read", "/w/b.rs"), &mut seen);
        read_line("not json \"tool_use\" \"file_path\"", &mut seen);
        assert_eq!(seen.files, HashSet::from(["/w/a.rs".to_string()]));
    }

    #[test]
    fn a_file_counts_for_the_shell_call_it_changed_in() {
        let dir = Git::new("shell");
        let mut seen = Seen::default();
        let at = |seconds: u32| format!("2026-11-06T10:00:{seconds:02}.000Z");
        let call = json!({"type": "assistant", "timestamp": at(10), "message": {"content": [{"type": "tool_use", "id": "b1", "name": "Bash", "input": {"command": "sed"}}]}});
        let result = json!({"type": "user", "timestamp": at(20), "message": {"content": [{"type": "tool_result", "tool_use_id": "b1", "content": "ok"}]}});
        read_line(&call.to_string(), &mut seen);
        read_line(&result.to_string(), &mut seen);
        assert_eq!(seen.shell.len(), 1);

        let touch = |name: &str, seconds: u32| {
            let path = dir.0.join(name);
            let file = std::fs::File::create(&path).expect("file");
            let time = chrono::DateTime::parse_from_rfc3339(&at(seconds)).expect("time");
            file.set_modified(time.into()).expect("time");
            path.to_string_lossy().into_owned()
        };
        assert!(seen.changed_by_shell(&touch("inside", 15)));
        assert!(!seen.changed_by_shell(&touch("before", 5)));
        assert!(!seen.changed_by_shell(&touch("after", 25)));
        assert!(!seen.changed_by_shell("/no/such/file"));
    }

    struct Git(PathBuf);

    impl Git {
        fn new(name: &str) -> Git {
            let dir = std::env::temp_dir().join(format!("pulse-work-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            Git(dir.canonicalize().expect("path"))
        }

        fn run(&self, dir: &str, args: &[&str]) {
            let at = self.0.join(dir);
            let done = Command::new("git").arg("-C").arg(at).args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().expect("git");
            assert!(done.status.success(), "{args:?}: {}", String::from_utf8_lossy(&done.stderr));
        }

        fn path(&self, dir: &str) -> String {
            self.0.join(dir).to_string_lossy().into_owned()
        }
    }

    impl Drop for Git {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_session_answers_for_its_own_files_and_branches() {
        let git = Git::new("real");
        std::fs::create_dir_all(git.0.join("work")).expect("dir");
        git.run("", &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        git.run("work", &["init", "-q", "-b", "main"]);
        git.run("work", &["remote", "add", "origin", &git.path("origin.git")]);
        std::fs::write(git.0.join("work/a.txt"), "1").expect("write");
        git.run("work", &["add", "."]);
        git.run("work", &["commit", "-qm", "one"]);
        git.run("work", &["push", "-q", "-u", "origin", "main"]);
        git.run("work", &["remote", "set-head", "origin", "main"]);
        git.run("work", &["switch", "-qc", "feature"]);
        std::fs::write(git.0.join("work/b.txt"), "2").expect("write");
        git.run("work", &["add", "."]);
        git.run("work", &["commit", "-qm", "two"]);
        // files of two sessions lie open in the folder
        std::fs::write(git.0.join("work/mine.txt"), "x").expect("write");
        std::fs::write(git.0.join("work/other.txt"), "x").expect("write");

        let work = git.path("work");
        let line = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Write", "input": {"file_path": format!("{work}/mine.txt")}}]}});
        std::fs::write(git.0.join("t.jsonl"), format!("{line}\n")).expect("transcript");
        let transcript = git.0.join("t.jsonl");
        let session = |id: &str, written: bool| Session {
            id: id.into(),
            cwd: Some(work.clone()),
            written: if written { vec![(work.clone(), "feature".to_string())] } else { Vec::new() },
            last_activity: Some(1000.0),
            ..Session::default()
        };

        let notes_now = |with_pr: &HashSet<&str>, now: f64| notes(&session("a", true), Some(&transcript), with_pr, now);
        assert_eq!(notes_now(&HashSet::new(), 1.0), vec!["1 file not committed", "1 commit on feature not pushed"]);
        // a session that wrote nothing here answers for nothing, however dirty the folder
        assert!(notes(&session("b", false), None, &HashSet::new(), 1.0).is_empty());

        // pushed: only the PR is missing; with a PR nothing is
        git.run("work", &["push", "-q", "origin", "feature"]);
        assert_eq!(notes_now(&HashSet::new(), 1.0 + 2.0 * GIT_FRESH_MS), vec!["1 file not committed", "feature has no PR"]);
        assert_eq!(notes_now(&HashSet::from(["feature"]), 1.0 + 2.0 * GIT_FRESH_MS), vec!["1 file not committed"]);

        // merged into the default branch: it has landed
        git.run("work", &["switch", "-q", "main"]);
        git.run("work", &["merge", "-q", "feature"]);
        git.run("work", &["push", "-q", "origin", "main"]);
        assert_eq!(notes_now(&HashSet::new(), 1.0 + 4.0 * GIT_FRESH_MS), vec!["1 file not committed"]);
    }
}
