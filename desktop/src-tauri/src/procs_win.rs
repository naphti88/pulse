// The commands' processes on Windows. Claude Code writes each command's output
// to %TEMP%\claude\<project>\<session>\tasks\<id>.output (or under
// CLAUDE_CODE_TMPDIR when set, or %TMP%); a file that cannot be opened without
// sharing write access is held open by a writer, and the file system names the
// processes that have it open (readers too). Their times and memory come from
// the process itself, their listening ports from the TCP tables.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, OsString};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Wdk::Storage::FileSystem::{FileProcessIdsUsingFileInformation, NtQueryInformationFile};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STATUS_BUFFER_OVERFLOW, STATUS_INFO_LENGTH_MISMATCH, STILL_ACTIVE};
use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_NATIVE, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::procs::{task_of, Found};
use crate::util::mtime_ms;

const SHARING_VIOLATION: i32 = 32;
// from 1601 to 1970 in 100 ns steps
const EPOCH_DIFF: u64 = 116_444_736_000_000_000;

// what the look before saw: when it ran, every file, and the files held, each
// with its holders as last named
struct Look {
    at: SystemTime,
    seen: HashSet<PathBuf>,
    held: HashMap<PathBuf, Named>,
}
static LOOK: Mutex<Option<Look>> = Mutex::new(None);
// A command's file is made as it starts, so a file the look before did not
// see is tried whatever its age, and one held then is tried as it may still
// be: a command silent for days stays found. Of the files seen before and not
// held only those made or written lately are tried, lately being generous for
// a writer that opens its file a while after it appears. After a gap longer
// than that since the look before (sleep, a clock set ahead or back) all are.
const FRESH: Duration = Duration::from_secs(10 * 60);

// a held file's holders when last named, and when that was: each process with
// when it was made, as its id may be another's later on; since when the file
// is known, for a look that names none; and the file itself, opened for its
// attributes when they were named, to ask through once its name cannot be
struct Named {
    at: SystemTime,
    pids: Vec<(u32, u64)>,
    since: SystemTime,
    file: Option<std::fs::File>,
}
// Claude Code deletes a call's file as the call returns, whatever the call
// left running (`npm run dev &`) still writes to it: the name goes at once
// (as Windows 10 and later delete), or stays but no longer opens until the
// last handle is closed (as of old; ours among them, so it goes the look
// after its last holder). A file held at the look before and so lost to its
// name is asked through the file kept, and its command stays while a holder
// is named. Kept for the files held at the look before only, one each (and a
// file gone as its command ends is named once more, the look after, to tell);
// a call that returns before a look names its file's holders (one quicker
// than the time between looks) is never seen, nor, once Pulse starts again,
// one whose file was gone by then: the file kept was all that told of it.
// Naming a file's holders costs far more than looking at a process, so those
// named are trusted this long, only checked as still running in between: a
// process that joins a command later (a shell's program, a dev server's node)
// shows up within it, or a few looks later when many are due at once.
const NAMED: Duration = Duration::from_secs(15);
// and as each naming walks every handle in the system (a few ms, half a second
// where handles run to millions, more under load), a look names those due
// again for this long at most (one at least), the longest unnamed first
const NAMING: Duration = Duration::from_secs(1);

fn to_probe(seen: bool, held: bool, touched: SystemTime, now: SystemTime, all: bool) -> bool {
    all || !seen || held || now.duration_since(touched).map_or(true, |age| age < FRESH)
}

// the temp folders Claude Code may write under: CLAUDE_CODE_TMPDIR (its own,
// read before the rest), %TEMP% (Node reads it first) and %TMP% (std's first),
// and std's own pick; each once, however it is written (its case, a last
// separator, a short name, a link to it)
fn roots() -> Vec<PathBuf> {
    roots_from(["CLAUDE_CODE_TMPDIR", "TEMP", "TMP"].map(std::env::var_os))
}

fn roots_from(set: [Option<OsString>; 3]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut known = HashSet::new();
    let set = set.into_iter().flatten().filter(|v| !v.is_empty()).map(PathBuf::from);
    for root in set.chain([std::env::temp_dir()]) {
        // where it really is, or as written when it is not there; looked in as written
        let real = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if known.insert(real.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase()) {
            roots.push(root);
        }
    }
    roots
}

// every <id>.output under the temp folders' claude and claude-* folders, with
// when it was made or last written, and when made; from the folder's listing,
// as opening each costs (a held file's writes may reach the listing only later)
fn outputs(roots: &[PathBuf]) -> Vec<(PathBuf, SystemTime, SystemTime)> {
    let mut list = Vec::new();
    let folders = roots.iter().flat_map(|root| std::fs::read_dir(root).into_iter().flatten().filter_map(Result::ok));
    for folder in folders.filter(|e| e.file_name().to_str().is_some_and(|n| n == "claude" || n.starts_with("claude-"))) {
        for project in children(&folder.path()) {
            for session in children(&project) {
                for file in std::fs::read_dir(session.join("tasks")).into_iter().flatten().filter_map(Result::ok) {
                    let path = file.path();
                    if !path.extension().is_some_and(|x| x == "output") {
                        continue;
                    }
                    let meta = file.metadata().ok();
                    let made = meta.as_ref().and_then(|m| m.created().ok());
                    let touched = made.max(meta.as_ref().and_then(|m| m.modified().ok())).unwrap_or_else(SystemTime::now);
                    list.push((path, touched, made.unwrap_or(touched)));
                }
            }
        }
    }
    list
}

fn children(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().filter_map(Result::ok).map(|e| e.path()).collect()
}

// a writer holds it: it does not open while others may write (letting the
// writer delete or rename it meanwhile). For the microseconds the probe is
// open (a few ms if an antivirus scans a file just written) it shares no
// writes either, so a writer opening the file just then fails; that takes a
// file no one holds yet and an open landing in that moment, rare enough to
// accept, as nothing else tells the writer among the holders named. None when
// its name cannot tell: gone, or deleted as of old (it no longer opens)
fn is_held(path: &Path) -> Option<bool> {
    match std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE).open(path) {
        Ok(_) => Some(false),
        Err(e) => (e.raw_os_error() == Some(SHARING_VIOLATION)).then_some(true),
    }
}

// the file opened for its attributes only, which takes no part in sharing: it
// neither fails for the writer's nor stands in anyone's way, nor keeps a
// delete from taking the name
fn attributes(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .ok()
}

// the processes that have the file open right now, readers too, but not this
// one, which the question's own handle makes one; none when it cannot be asked
fn holders(file: &std::fs::File) -> Option<Vec<u32>> {
    // a count, then the ids from the next word on (a buffer of words, aligned as they are)
    let mut list = vec![0usize; 64];
    for _ in 0..8 {
        let mut status = IO_STATUS_BLOCK::default();
        let size = std::mem::size_of_val(list.as_slice()) as u32;
        // SAFETY: the handle is open while `file` lives; the buffer is `size` bytes
        let result = unsafe { NtQueryInformationFile(file.as_raw_handle(), &mut status, list.as_mut_ptr() as *mut c_void, size, FileProcessIdsUsingFileInformation) };
        if result == STATUS_INFO_LENGTH_MISMATCH || result == STATUS_BUFFER_OVERFLOW {
            list = vec![0; list.len() * 2];
            continue;
        }
        if result < 0 {
            return None;
        }
        // the count is the first word's low half
        let count = list[0] as u32 as usize;
        let me = std::process::id();
        return Some(list[1..].iter().take(count).map(|&pid| pid as u32).filter(|&pid| pid != me).collect());
    }
    None
}

fn ticks(t: &FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

// a moment in FILETIME ticks
fn ticks_at(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| (d.as_nanos() / 100) as u64) + EPOCH_DIFF
}

fn ms(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

// this program's file name, in lower case: another Pulse's (an installed one
// beside a build) is the same
static PROGRAM: LazyLock<Option<String>> = LazyLock::new(|| Some(std::env::current_exe().ok()?.file_name()?.to_string_lossy().to_lowercase()));

// when the process was made (FILETIME ticks), its CPU time (ns), memory in
// use, and whether it runs this program; none when it is gone, has ended or
// is closed to this user
fn process_info(pid: u32) -> Option<(u64, f64, u64, bool)> {
    // SAFETY: a handle for queries only, closed below
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    let mut memory: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let mut code = 0u32;
    let mut program = [0u16; 1024];
    let mut length = program.len() as u32;
    // SAFETY: the handle is open; each out pointer is a struct of its type, the
    // program's buffer `length` characters
    let running = unsafe { GetExitCodeProcess(handle, &mut code) } != 0 && code == STILL_ACTIVE as u32;
    let times = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0;
    let has_memory = unsafe { K32GetProcessMemoryInfo(handle, &mut memory, size) } != 0;
    let has_program = unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_NATIVE, program.as_mut_ptr(), &mut length) } != 0;
    unsafe { CloseHandle(handle) };
    let cpu_ns = (ticks(&kernel) + ticks(&user)) as f64 * 100.0;
    let program = String::from_utf16_lossy(&program[..length as usize]);
    let is_pulse = has_program && PROGRAM.as_deref().is_some_and(|me| program.rsplit('\\').next().is_some_and(|name| name.to_lowercase() == me));
    (running && times).then(|| (ticks(&created), cpu_ns, if has_memory { memory.WorkingSetSize as u64 } else { 0 }, is_pulse))
}

// a holder: its id, when it was made, its CPU time and memory
type Holder = (u32, u64, f64, u64);

// the file's holders named now, those that can be looked at; an id whose
// process was made after the question is another process's by now (and so
// seems one made in the span a clock set back repeats, until it catches up),
// and another Pulse holds every file held through the one it keeps: neither
// is the command's (nor, so, is a Pulse that a command runs)
fn named_now(file: &std::fs::File) -> Vec<Holder> {
    let asked = ticks_at(SystemTime::now());
    let pids = holders(file).unwrap_or_default();
    pids.into_iter().filter_map(|pid| process_info(pid).filter(|p| p.0 <= asked && !p.3).map(|(made, cpu_ns, memory, _)| (pid, made, cpu_ns, memory))).collect()
}

// the holders named before that still run, each the same process as then
fn still_running(pids: &[(u32, u64)]) -> Vec<Holder> {
    pids.iter().filter_map(|&(pid, made)| process_info(pid).filter(|p| p.0 == made).map(|(_, cpu_ns, memory, _)| (pid, made, cpu_ns, memory))).collect()
}

// one TCP table of listening sockets with their owners, as raw bytes
fn tcp_table(family: u16) -> Vec<u8> {
    let mut size = 0u32;
    // SAFETY: a null buffer asks for the size
    unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family as u32, TCP_TABLE_OWNER_PID_LISTENER, 0) };
    for _ in 0..3 {
        let mut buffer = vec![0u8; size as usize + 1024];
        size = buffer.len() as u32;
        // SAFETY: the buffer is `size` bytes long
        let status = unsafe { GetExtendedTcpTable(buffer.as_mut_ptr() as *mut c_void, &mut size, 0, family as u32, TCP_TABLE_OWNER_PID_LISTENER, 0) };
        if status == 0 {
            buffer.truncate(size as usize);
            return buffer;
        }
    }
    Vec::new()
}

// the rows that follow the table's count
fn rows<T: Copy>(table: &[u8]) -> Vec<T> {
    if table.len() < 4 {
        return Vec::new();
    }
    let count = u32::from_ne_bytes(table[..4].try_into().expect("4 bytes")) as usize;
    let at = std::mem::align_of::<T>().max(4);
    let size = std::mem::size_of::<T>();
    (0..count)
        .take_while(|i| at + (i + 1) * size <= table.len())
        // SAFETY: inside the buffer; read unaligned as the buffer is bytes
        .map(|i| unsafe { std::ptr::read_unaligned(table[at + i * size..].as_ptr() as *const T) })
        .collect()
}

// by process: the TCP ports it listens on
fn listening() -> HashMap<u32, Vec<u16>> {
    let mut ports: HashMap<u32, Vec<u16>> = HashMap::new();
    let port = |raw: u32| u16::from_be(raw as u16);
    for row in rows::<MIB_TCPROW_OWNER_PID>(&tcp_table(AF_INET)) {
        ports.entry(row.dwOwningPid).or_default().push(port(row.dwLocalPort));
    }
    for row in rows::<MIB_TCP6ROW_OWNER_PID>(&tcp_table(AF_INET6)) {
        ports.entry(row.dwOwningPid).or_default().push(port(row.dwLocalPort));
    }
    ports
}

pub fn scan() -> HashMap<(String, String), Found> {
    scan_in(&roots(), NAMING)
}

// the look over these temp folders, naming those due again for `naming` at most
fn scan_in(roots: &[PathBuf], naming: Duration) -> HashMap<(String, String), Found> {
    let mut found: HashMap<(String, String), Found> = HashMap::new();
    // one look at a time, its naming too: a look that waits for it then trusts
    // what it named rather than naming it all again alongside
    let mut state = LOOK.lock().unwrap_or_else(PoisonError::into_inner);
    let begun = Instant::now();
    let now = SystemTime::now();
    let last = state.take();
    let all = last.as_ref().map_or(true, |l| now.duration_since(l.at).map_or(true, |gap| gap > FRESH));
    let (seen_before, mut before) = last.map(|l| (l.seen, l.held)).unwrap_or_default();
    let files = outputs(roots);
    let seen: HashSet<PathBuf> = files.iter().map(|(p, _, _)| p.clone()).collect();
    // those held, each asked by its name or not: held by a writer as its name
    // tells, or held before while its name no longer tells, asked through the
    // file kept
    let mut held: Vec<(PathBuf, (String, String), SystemTime, bool)> = files
        .into_iter()
        .filter(|(p, touched, _)| to_probe(seen_before.contains(p), before.contains_key(p), *touched, now, all))
        .filter_map(|(p, _, made)| task_of(&p.to_string_lossy()).map(|t| (p, t, made)))
        .filter_map(|(p, t, made)| match is_held(&p) {
            Some(true) => Some((p, t, made, true)),
            Some(false) => None,
            None => before.contains_key(&p).then_some((p, t, made, false)),
        })
        .collect();
    // and those held before and gone from the listing since, deleted while
    // held, under these temp folders: a look over others (a test's) tells
    // nothing of them, and Pulse's own stay the same while it runs, whatever
    // sessions come and go in them
    held.extend(
        before
            .iter()
            .filter(|(p, _)| !seen.contains(*p) && roots.iter().any(|root| p.starts_with(root)))
            .filter_map(|(p, n)| task_of(&p.to_string_lossy()).map(|t| (p.clone(), t, n.since, false))),
    );
    // those never named first, then those named longest ago
    held.sort_by_key(|(p, _, _, _)| before.get(p).map(|n| n.at));
    let ports = if held.is_empty() { HashMap::new() } else { listening() };
    let mut named = HashMap::new();
    // the commands found to have ended: none of their processes, the file no longer held
    let mut ended = HashSet::new();
    // those named a while ago and named again in this look
    let mut again = 0;
    for (path, key, made, by_name) in held {
        let last = before.remove(&path);
        let since = last.as_ref().map_or(made, |n| n.since);
        // those named before that still run; named anew when the file is newly
        // held or none of them still runs, and when they were named a while
        // ago while this look has taken under `naming` (once at least), the
        // rest trusted until a look after
        let mut running = last.as_ref().map(|n| still_running(&n.pids)).unwrap_or_default();
        let (mut at, mut file) = last.map_or((now, None), |n| (n.at, n.file));
        let lately = now.duration_since(at).is_ok_and(|age| age < NAMED);
        if running.is_empty() || (!lately && (again == 0 || begun.elapsed() < naming)) {
            again += usize::from(!running.is_empty());
            // trusted from when this file's are asked for, not from when the
            // look began: those named late in a long look are as fresh
            at = SystemTime::now();
            // by its name while it has one (through the file kept should it
            // go meanwhile), else through the file kept
            if by_name {
                file = attributes(&path).or(file);
            }
            running = file.as_ref().map(named_now).unwrap_or_default();
        }
        let one = found.entry(key.clone()).or_default();
        // its processes' CPU read just now, which may be a while into the look
        one.at = ms(SystemTime::now());
        for &(pid, created, cpu_ns, memory) in &running {
            // a folder listed under two spellings names them twice
            if one.pids.contains(&(pid as i32)) {
                continue;
            }
            let started = created.saturating_sub(EPOCH_DIFF) as f64 / 10_000.0;
            if one.pids.is_empty() || started < one.started {
                one.started = started;
            }
            one.pids.push(pid as i32);
            one.cpu_ns += cpu_ns;
            one.memory += memory;
            one.ports.extend(ports.get(&pid).into_iter().flatten());
        }
        // none of its processes named (the question failed, or they are gone
        // or closed to this user): while its file is still held its command
        // still runs, so it stays, without pids, from when its file was made;
        // only a file no longer held ends it, and one asked through the file
        // kept is held while a holder is named
        if one.pids.is_empty() {
            if !by_name || is_held(&path) == Some(false) {
                ended.insert(key);
                continue;
            }
            one.started = ms(since);
        }
        // left running by a call that returned: from when its file was made,
        // and last written as the file kept tells, its name gone
        if !by_name {
            one.started = ms(since);
            one.left = Some(file.as_ref().and_then(|f| f.metadata().ok()).map_or(one.started, |m| mtime_ms(&m)));
        }
        one.path = path.to_string_lossy().into_owned();
        named.insert(path, Named { at, pids: running.iter().map(|&(pid, created, _, _)| (pid, created)).collect(), since, file });
    }
    *state = Some(Look { at: now, seen, held: named });
    // a file held by a process that is gone by now, and no longer held
    found.retain(|key, one| !one.pids.is_empty() || !ended.contains(key));
    found
}

// a test's own temp folder, one per run, and the children it starts: all gone
// when the test ends, passed or failed
#[cfg(test)]
pub struct Fixture {
    pub root: PathBuf,
    pub children: Vec<std::process::Child>,
}

#[cfg(test)]
impl Fixture {
    pub fn new(name: &str) -> Fixture {
        // its looks its own, nothing carried from a look before (its callers take turns)
        *LOOK.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let root = std::env::temp_dir().join(format!("pulse-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        Fixture { root, children: Vec::new() }
    }

    // a session's task folder in this temp folder's claude folder
    pub fn tasks(&self, project: &str, session: &str) -> PathBuf {
        let dir = self.root.join("claude").join(project).join(session).join("tasks");
        std::fs::create_dir_all(&dir).expect("tasks");
        dir
    }

    // a child that holds the file (its output) for half a minute; its id
    pub fn ping(&mut self, output: std::fs::File) -> i32 {
        let child = std::process::Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(output).spawn().expect("child");
        self.children.push(child);
        self.children.last().map_or(0, |c| c.id() as i32)
    }

    pub fn look(&self) -> HashMap<(String, String), Found> {
        scan_in(std::slice::from_ref(&self.root), NAMING)
    }

    pub fn end(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
impl Drop for Fixture {
    fn drop(&mut self) {
        self.end();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::LOOKING;

    // a file new to the look is tried however old; one seen and not held only
    // while fresh, or after a gap
    #[test]
    fn which_files_are_tried() {
        let now = SystemTime::now();
        let old = now - 2 * FRESH;
        assert!(to_probe(false, false, old, now, false));
        assert!(!to_probe(true, false, old, now, false));
        assert!(to_probe(true, true, old, now, false));
        assert!(to_probe(true, false, now - FRESH / 2, now, false));
        assert!(to_probe(true, false, now + FRESH, now, false));
        assert!(to_probe(true, false, old, now, true));
    }

    fn pids(found: &HashMap<(String, String), Found>, session: &str, id: &str) -> Vec<i32> {
        let mut pids = found.get(&(session.to_string(), id.to_string())).map(|one| one.pids.clone()).unwrap_or_default();
        pids.sort_unstable();
        pids
    }

    // as if the holders named for these files (all, given none) were named a while ago
    fn named_long_ago(paths: &[&Path]) {
        if let Some(look) = LOOK.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            for (_, named) in look.held.iter_mut().filter(|(p, _)| paths.is_empty() || paths.contains(&p.as_path())) {
                named.at -= NAMED;
            }
        }
    }

    // when the look before began, and when it named these files' holders
    fn named_at(paths: &[&Path]) -> (SystemTime, Vec<SystemTime>) {
        let look = LOOK.lock().unwrap_or_else(PoisonError::into_inner);
        let look = look.as_ref().expect("a look");
        (look.at, paths.iter().map(|p| look.held[*p].at).collect())
    }

    // two commands of this test, each a child that holds its own file, and a
    // child that joins the first later on (as a shell starts a program): each
    // file is told its own holders, the late one once those named before are
    // named again
    #[test]
    fn each_file_has_its_own_holders() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("holders");
        let session = "00000000-0000-4000-8000-000000000002";
        let dir = test.tasks("p", session);
        let (path_a, path_b) = (dir.join("a.output"), dir.join("b.output"));
        let a = test.ping(std::fs::File::create(&path_a).expect("a"));
        let b = test.ping(std::fs::File::create(&path_b).expect("b"));

        let found = test.look();
        assert_eq!(pids(&found, session, "a"), vec![a]);
        assert_eq!(pids(&found, session, "b"), vec![b]);
        // each file's holders trusted from when they were named, one after
        // the other, not all from when the look began
        let (began, at) = named_at(&[&path_a, &path_b]);
        assert!(at.iter().all(|&t| t > began) && at[0] != at[1], "began {began:?}, named {at:?}");
        // and each command's CPU dated when it was read: once its own file's
        // holders were named, not as the look began
        let read = |found: &HashMap<(String, String), Found>, id: &str| found[&(session.to_string(), id.to_string())].at;
        assert!(read(&found, "a") >= ms(at[0]) && read(&found, "b") >= ms(at[1]), "named {at:?}, read {} and {}", read(&found, "a"), read(&found, "b"));

        // the handle passes to the child as it is made, so it holds the file once spawned
        let late = test.ping(std::fs::OpenOptions::new().append(true).open(&path_a).expect("a again"));
        // those named lately are trusted, until named again, their CPU still
        // dated in the look that read it
        let trusted = test.look();
        assert_eq!(pids(&trusted, session, "a"), vec![a]);
        let began = named_at(&[]).0;
        assert!(trusted.values().all(|f| f.at >= ms(began)), "began {}, read {:?}", ms(began), trusted.values().map(|f| f.at).collect::<Vec<_>>());
        named_long_ago(&[&path_a]);
        let found = test.look();
        let mut both = vec![a, late];
        both.sort_unstable();
        assert_eq!(pids(&found, session, "a"), both);
        assert_eq!(pids(&found, session, "b"), vec![b]);

        test.end();
        // no longer held: gone
        assert!(test.look().is_empty());
    }

    // a file held where no holder can be named (here by this process, which
    // the question leaves out): the command stays, without pids, from when its
    // file was made, and is gone once the file is no longer held
    #[test]
    fn a_held_file_without_named_holders_is_kept() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let test = Fixture::new("kept");
        let session = "00000000-0000-4000-8000-000000000003";
        let path = test.tasks("p", session).join("k.output");
        let file = std::fs::File::create(&path).expect("k");
        let made = ms(std::fs::metadata(&path).expect("meta").created().expect("made"));
        let key = (session.to_string(), "k".to_string());
        for _ in 0..2 {
            let found = test.look();
            let one = found.get(&key).expect("kept while its file is held");
            assert!(one.pids.is_empty(), "{:?}", one.pids);
            assert_eq!(one.started, made);
        }
        drop(file);
        assert!(!test.look().contains_key(&key), "no longer held: gone");
    }

    // a held file deleted while its holder runs on (as Claude Code deletes a
    // call's file once the call returns): the command stays, asked through the
    // file kept, last written as that tells, until no holder remains
    #[test]
    fn a_held_file_deleted_is_kept_while_held() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("deleted");
        let session = "00000000-0000-4000-8000-000000000007";
        let path = test.tasks("p", session).join("d.output");
        let pid = test.ping(std::fs::File::create(&path).expect("d"));
        let key = (session.to_string(), "d".to_string());
        let found = test.look();
        assert_eq!(pids(&found, session, "d"), vec![pid]);
        assert!(found[&key].left.is_none(), "found by its name: not left");
        std::fs::remove_file(&path).expect("deleted while held");
        let deleted = ms(SystemTime::now());
        assert!(!path.exists(), "its name gone");
        // written again since (a line a second), and due again, so asked
        std::thread::sleep(Duration::from_millis(1200));
        named_long_ago(&[]);
        let found = test.look();
        assert_eq!(pids(&found, session, "d"), vec![pid]);
        assert!(found[&key].left.is_some_and(|written| written > deleted), "deleted {deleted}, written {:?}", found[&key].left);
        test.end();
        assert!(!test.look().contains_key(&key), "no holder left: gone");
    }

    // another Pulse looking in the same folders holds every held file through
    // the one it keeps (here a copy of this test's own program, in another
    // folder and its name in another case, as an installed Pulse beside a
    // build may be, holding it so): it is none of the command's processes, nor
    // keeps the command once its file is deleted and its own processes are gone
    #[test]
    fn another_pulse_is_no_holder() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("pulse");
        let session = "00000000-0000-4000-8000-00000000000a";
        let path = test.tasks("p", session).join("p.output");
        let pid = test.ping(std::fs::File::create(&path).expect("p"));
        let key = (session.to_string(), "p".to_string());
        // the copy gone with the fixture's folder
        let program = std::env::current_exe().expect("this program");
        let copy = test.root.join("other").join(program.file_name().expect("its name").to_string_lossy().to_uppercase());
        std::fs::create_dir_all(test.root.join("other")).expect("other");
        std::fs::copy(&program, &copy).expect("a copy");
        let other = std::process::Command::new(&copy)
            .args(["--ignored", "--exact", "procs_win::tests::held_as_another_pulse"])
            .env("PULSE_TEST_HOLD", &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("another");
        let pulse = other.id();
        test.children.push(other);
        let file = attributes(&path).expect("p");
        let holding = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            holders(&file).is_some_and(|pids| pids.contains(&pulse))
        });
        assert!(holding, "the other holds it");
        assert_eq!(pids(&test.look(), session, "p"), vec![pid]);
        let _ = test.children[0].kill();
        let _ = test.children[0].wait();
        std::fs::remove_file(&path).expect("deleted while held");
        named_long_ago(&[]);
        assert!(holders(&file).is_some_and(|pids| pids == [pulse]), "held by the other only");
        assert!(!test.look().contains_key(&key), "no holder but the other: gone");
    }

    // the other Pulse of the test above, holding its file as a Pulse does
    // until it is ended (and nothing, run without it)
    #[test]
    #[ignore]
    fn held_as_another_pulse() {
        let Some(path) = std::env::var_os("PULSE_TEST_HOLD") else { return };
        let _file = attributes(Path::new(&path)).expect("held");
        std::thread::sleep(Duration::from_secs(30));
    }

    // the same deleted as of old, its name kept until its last handle is
    // closed but no longer opening: kept while held, then gone, the name too
    // as the file kept is closed
    #[test]
    fn a_held_file_deleted_as_of_old_is_kept_while_held() {
        use windows_sys::Win32::Storage::FileSystem::{FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO};
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("pending");
        let session = "00000000-0000-4000-8000-000000000008";
        let dir = test.tasks("p", session);
        let path = dir.join("d.output");
        let pid = test.ping(std::fs::File::create(&path).expect("d"));
        let key = (session.to_string(), "d".to_string());
        assert_eq!(pids(&test.look(), session, "d"), vec![pid]);
        let file = std::fs::OpenOptions::new().access_mode(DELETE).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(&path).expect("to delete");
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open; the buffer is the struct, its size given
        let set = unsafe { SetFileInformationByHandle(file.as_raw_handle(), FileDispositionInfo, &info as *const _ as *const c_void, std::mem::size_of_val(&info) as u32) };
        drop(file);
        assert!(set != 0 && children(&dir) == [path.clone()] && is_held(&path).is_none(), "listed, no longer opening");
        named_long_ago(&[]);
        assert_eq!(pids(&test.look(), session, "d"), vec![pid]);
        test.end();
        assert!(!test.look().contains_key(&key), "no holder left: gone");
        assert!(children(&dir).is_empty(), "its name gone with the file kept");
    }

    // the call's own processes gone as it returns and its file deleted, what
    // it left running joined after its holders were named (a dev server
    // started in the background of a call): found through the file kept
    #[test]
    fn what_a_call_left_running_is_found_once_its_file_is_deleted() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("left");
        let session = "00000000-0000-4000-8000-000000000009";
        let path = test.tasks("p", session).join("l.output");
        let call = test.ping(std::fs::File::create(&path).expect("l"));
        let key = (session.to_string(), "l".to_string());
        assert_eq!(pids(&test.look(), session, "l"), vec![call]);
        let left = test.ping(std::fs::OpenOptions::new().append(true).open(&path).expect("l again"));
        let _ = test.children[0].kill();
        let _ = test.children[0].wait();
        std::fs::remove_file(&path).expect("deleted while held");
        assert_eq!(pids(&test.look(), session, "l"), vec![left]);
        test.end();
        assert!(!test.look().contains_key(&key), "no holder left: gone");
    }

    // a file under a path longer than MAX_PATH is named its holder all the same
    #[test]
    fn a_long_path_is_named_its_holder() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("long");
        let session = "00000000-0000-4000-8000-000000000004";
        let path = test.tasks(&"x".repeat(200), session).join("l.output");
        assert!(path.as_os_str().len() > 260, "{} characters", path.as_os_str().len());
        let pid = test.ping(std::fs::File::create(&path).expect("l"));
        assert_eq!(pids(&test.look(), session, "l"), vec![pid]);
    }

    // CLAUDE_CODE_TMPDIR, TEMP and TMP set apart: the files under each are
    // found; one folder written two ways (its case and a last separator, or
    // \\?\ before it) is looked in once
    #[test]
    fn each_temp_folder_is_looked_in() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut set = [Fixture::new("own"), Fixture::new("temp"), Fixture::new("tmp")];
        let session = "00000000-0000-4000-8000-000000000005";
        let mut ids = Vec::new();
        for (test, id) in set.iter_mut().zip(["o", "a", "b"]) {
            let file = std::fs::File::create(test.tasks("p", session).join(format!("{id}.output"))).expect("file");
            ids.push((id, test.ping(file)));
        }
        let root = |n: usize| Some(set[n].root.clone().into_os_string());
        let roots = roots_from([root(0), root(1), root(2)]);
        assert_eq!(roots[..3], [set[0].root.clone(), set[1].root.clone(), set[2].root.clone()]);
        let found = scan_in(&roots[..3], NAMING);
        for (id, pid) in ids {
            assert_eq!(pids(&found, session, id), vec![pid], "{id}");
        }
        let temp = &set[1].root;
        let upper = format!("{}\\", temp.to_string_lossy().to_uppercase());
        let verbatim = format!(r"\\?\{}", temp.display());
        for again in [upper, verbatim] {
            assert_eq!(roots_from([None, root(1), Some(again.clone().into())]), roots_from([None, root(1), None]), "{again}");
        }
    }

    // what a look costs with ten commands running besides whatever the
    // machine runs (theirs in this test's own folder, out of sight of a Pulse
    // running alongside): cargo test the_cost_of_a_look -- --ignored --nocapture
    #[test]
    #[ignore]
    fn the_cost_of_a_look() {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("cost");
        let session = "00000000-0000-4000-8000-0000000000c0";
        let dir = test.tasks("p", session);
        for n in 0..10 {
            let file = std::fs::File::create(dir.join(format!("c{n}.output"))).expect("file");
            test.children.push(std::process::Command::new("ping").args(["-n", "120", "127.0.0.1"]).stdout(file).spawn().expect("child"));
        }
        let roots: Vec<PathBuf> = roots().into_iter().chain([test.root.clone()]).collect();
        let cpu_ms = || {
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            // SAFETY: this process's own pseudo handle; each out pointer is a FILETIME
            unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
            (ticks(&kernel) + ticks(&user)) as f64 / 10_000.0
        };
        let timed = || {
            let (t, cpu) = (std::time::Instant::now(), cpu_ms());
            let found = scan_in(&roots, NAMING);
            let mine = found.keys().filter(|(s, _)| s == session).count();
            (t.elapsed().as_secs_f64() * 1000.0, cpu_ms() - cpu, found.len(), mine)
        };
        let (wall, cpu, all, mine) = timed();
        println!("COST first look: {wall:.1} ms wall, {cpu:.1} ms CPU, {all} commands ({mine} of this test)");
        let mut after: Vec<(f64, f64)> = (0..5).map(|_| timed()).map(|(w, c, _, _)| (w, c)).collect();
        after.sort_by(|a, b| a.0.total_cmp(&b.0));
        let line: Vec<String> = after.iter().map(|(w, c)| format!("{w:.1}/{c:.1}")).collect();
        println!("COST looks after (ms wall/CPU, sorted): {}", line.join(" "));
        named_long_ago(&[]);
        let line: Vec<String> = (0..8).map(|_| timed()).map(|(w, c, _, _)| format!("{w:.1}/{c:.1}")).collect();
        println!("COST looks once all {all} are due again (ms wall/CPU, in turn): {}", line.join(" "));
    }

    // a look names those due again only while it has taken under its time
    // (here none, so once), the longest unnamed first: two late holders, each
    // shown a look after the other, the one whose file was named longer ago first
    #[test]
    fn those_named_longest_ago_are_named_again_first() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = Fixture::new("due");
        let session = "00000000-0000-4000-8000-000000000006";
        let dir = test.tasks("p", session);
        let (path_a, path_b) = (dir.join("a.output"), dir.join("b.output"));
        let a = test.ping(std::fs::File::create(&path_a).expect("a"));
        let b = test.ping(std::fs::File::create(&path_b).expect("b"));
        let once = |test: &Fixture| scan_in(std::slice::from_ref(&test.root), Duration::ZERO);
        // newly held, both named, whatever the time
        let found = once(&test);
        assert_eq!((pids(&found, session, "a"), pids(&found, session, "b")), (vec![a], vec![b]));
        let late_a = test.ping(std::fs::OpenOptions::new().append(true).open(&path_a).expect("a again"));
        let late_b = test.ping(std::fs::OpenOptions::new().append(true).open(&path_b).expect("b again"));
        // both due again, b's named longer ago
        named_long_ago(&[&path_a, &path_b]);
        named_long_ago(&[&path_b]);
        let sorted = |mut v: Vec<i32>| {
            v.sort_unstable();
            v
        };
        let found = once(&test);
        assert_eq!((pids(&found, session, "a"), pids(&found, session, "b")), (vec![a], sorted(vec![b, late_b])));
        let found = once(&test);
        assert_eq!((pids(&found, session, "a"), pids(&found, session, "b")), (sorted(vec![a, late_a]), sorted(vec![b, late_b])));
    }
}
