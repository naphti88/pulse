// The commands the sessions run right now, from the process table: every
// command a session starts, in the foreground or the background, writes to
// <temp>/claude[-<uid>]/<project>/<session>/tasks/<id>.output and holds that
// file open while it runs; the processes that hold it are the command (the
// shell and what it started). On macOS through libproc (procs_mac.rs), on
// Windows through the file system, which names the processes that have a file
// open (procs_win.rs).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

#[cfg(target_os = "macos")]
use crate::procs_mac::scan;
#[cfg(windows)]
use crate::procs_win::scan;
use crate::util::mtime_ms;

// no process table read elsewhere: no commands
#[cfg(not(any(target_os = "macos", windows)))]
fn scan() -> HashMap<(String, String), Found> {
    HashMap::new()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    // the session's id in the transcript's name, and the command's own id
    pub session: String,
    pub id: String,
    pub started: f64,
    // when it last wrote something
    pub last_output: f64,
    // cores in use since the look before (1.0 is one core fully)
    pub cpu: f64,
    // since when it uses most of a core without a break
    pub hot_since: Option<f64>,
    pub memory: u64,
    pub ports: Vec<u16>,
    // left running by a call that returned: no one waits for it
    pub left: bool,
}

// a command's processes (none when its file is held but they could not be
// named), with when the first started, their CPU time and when it was read (0
// for as the look began), memory and ports; for one left running by a call
// that returned, its file deleted (procs_win.rs), when it last wrote
#[derive(Default)]
pub struct Found {
    pub pids: Vec<i32>,
    pub started: f64,
    pub cpu_ns: f64,
    pub at: f64,
    pub memory: u64,
    pub ports: Vec<u16>,
    pub path: String,
    pub left: Option<f64>,
}

// "<session>/tasks/<id>.output" at the end of a path under claude or claude-<uid>
pub fn task_of(path: &str) -> Option<(String, String)> {
    let path = path.replace('\\', "/");
    if !path.split('/').any(|part| part == "claude" || part.starts_with("claude-")) {
        return None;
    }
    let mut parts = path.rsplit('/');
    let file = parts.next()?;
    let id = file.strip_suffix(".output")?;
    if parts.next()? != "tasks" {
        return None;
    }
    let session = parts.next()?;
    let is_uuid = session.len() == 36 && session.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    (is_uuid && !id.is_empty()).then(|| (session.to_string(), id.to_string()))
}

// what the look before saw of each command: when, its CPU time, since when
// hot, and the memory and ports it was last measured with
struct Sample {
    at: f64,
    cpu_ns: f64,
    hot_since: Option<f64>,
    memory: u64,
    ports: Vec<u16>,
}
static LAST: LazyLock<Mutex<HashMap<String, Sample>>> = LazyLock::new(Default::default);

// a look this short after the one before measures too little to say hot
const MIN_SAMPLE_MS: f64 = 20_000.0;
// most of a core
const HOT: f64 = 0.8;

// the tests that look (sharing the looks before) take turns
#[cfg(test)]
pub static LOOKING: Mutex<()> = Mutex::new(());

pub fn commands(now: f64) -> Vec<Command> {
    sample(scan(), now)
}

// the commands of a look, each with its CPU since the look before
fn sample(found: HashMap<(String, String), Found>, now: f64) -> Vec<Command> {
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    let mut seen = HashMap::new();
    let mut out = Vec::new();
    for ((session, id), mut one) in found {
        let key = format!("{session}/{id}");
        one.ports.sort_unstable();
        one.ports.dedup();
        // the CPU over the time between its reads: a look that names holders
        // reads some a while after it began
        let at = if one.at > 0.0 { one.at } else { now };
        let (cpu, hot_since, sample) = match last.remove(&key) {
            // none of its processes named this time: nothing measured, the
            // look before kept for the next that measures, and the memory and
            // ports it saw
            Some(s) if one.pids.is_empty() => {
                (one.memory, one.ports) = (s.memory, s.ports.clone());
                (0.0, s.hot_since, Some(s))
            }
            None if one.pids.is_empty() => (0.0, None, None),
            Some(s) if at - s.at < MIN_SAMPLE_MS => (0.0, s.hot_since, Some(Sample { memory: one.memory, ports: one.ports.clone(), ..s })),
            Some(s) => {
                let cpu = ((one.cpu_ns - s.cpu_ns) / 1e6 / (at - s.at)).max(0.0);
                let hot = if cpu >= HOT { Some(s.hot_since.unwrap_or(s.at)) } else { None };
                (cpu, hot, Some(Sample { at, cpu_ns: one.cpu_ns, hot_since: hot, memory: one.memory, ports: one.ports.clone() }))
            }
            None => (0.0, None, Some(Sample { at, cpu_ns: one.cpu_ns, hot_since: None, memory: one.memory, ports: one.ports.clone() })),
        };
        if let Some(sample) = sample {
            seen.insert(key, sample);
        }
        // never from 1970: a start not known is taken as now
        let started = if one.started > 0.0 { one.started } else { now };
        let last_output = one.left.or_else(|| std::fs::metadata(&one.path).ok().map(|m| mtime_ms(&m))).unwrap_or(started);
        out.push(Command { session, id, started, last_output, cpu, hot_since, memory: one.memory, ports: one.ports, left: one.left.is_some() });
    }
    *last = seen;
    out.sort_by(|a, b| a.started.total_cmp(&b.started));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_of_an_output_path() {
        let session = "7e3c51a2-0b4d-4c8e-9f61-2a5d8b0c4e17";
        let path = format!("/private/tmp/claude-501/-Users-me-Work-shop-api/{session}/tasks/b1x2y3z4w.output");
        assert_eq!(task_of(&path), Some((session.to_string(), "b1x2y3z4w".to_string())));
        assert_eq!(task_of("/private/tmp/claude-501/x/tasks/a.output"), None);
        assert_eq!(task_of(&format!("/tmp/other/{session}/tasks/a.output")), None);
        assert_eq!(task_of(&format!("/private/tmp/claude-501/p/{session}/tasks/a.log")), None);
        let windows = format!(r"C:\Users\me\AppData\Local\Temp\claude\C--Users-me-shop\{session}\tasks\b1x2y3z4w.output");
        assert_eq!(task_of(&windows), Some((session.to_string(), "b1x2y3z4w".to_string())));
    }

    // a look that names none of a command's processes measures nothing: no
    // CPU then, nor a made-up burst after it (its whole CPU time as used
    // meanwhile), the memory and ports as last measured, and never a start in 1970
    #[test]
    fn a_look_without_pids_keeps_the_sample() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let key = ("00000000-0000-4000-8000-0000000000aa".to_string(), "c".to_string());
        let t0 = 1_792_000_000_000.0;
        let measured = |cpu_s: f64| Found { pids: vec![1], started: t0 - 600_000.0, cpu_ns: cpu_s * 1e9, memory: 5 << 20, ports: vec![3000], ..Default::default() };
        let unmeasured = || Found { started: t0 - 600_000.0, ..Default::default() };
        sample(HashMap::from([(key.clone(), measured(300.0))]), t0);
        let mid = sample(HashMap::from([(key.clone(), unmeasured())]), t0 + 30_000.0);
        assert_eq!((mid[0].cpu, mid[0].memory, mid[0].ports.clone()), (0.0, 5 << 20, vec![3000]));
        let after = sample(HashMap::from([(key.clone(), measured(301.0))]), t0 + 60_000.0);
        assert!(after[0].cpu < 0.05, "{} cores", after[0].cpu);
        assert_eq!(after[0].hot_since, None);
        let untold = sample(HashMap::from([(key, Found::default())]), t0);
        assert_eq!(untold[0].started, t0);
    }

    // a look that names holders reads a command's CPU a while after it began:
    // the CPU is over the time between the reads, not between the looks' starts
    #[test]
    fn the_cpu_is_over_the_time_between_reads() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let key = ("00000000-0000-4000-8000-0000000000ab".to_string(), "c".to_string());
        let t0 = 1_792_000_000_000.0;
        let read = |cpu_s: f64, at: f64| Found { pids: vec![1], started: t0, cpu_ns: cpu_s * 1e9, at, ..Default::default() };
        sample(HashMap::from([(key.clone(), read(100.0, t0))]), t0);
        // begun 30 s on and read 10 s into it: 40 s of CPU in 40 s, one core
        let then = sample(HashMap::from([(key, read(140.0, t0 + 40_000.0))]), t0 + 30_000.0);
        assert!((then[0].cpu - 1.0).abs() < 1e-9, "{} cores", then[0].cpu);
    }

    // one left running by a call that returned is told so, its last output
    // when its file was last written, its file having no name to look up
    #[test]
    fn what_a_call_left_running_is_told_so() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let key = ("00000000-0000-4000-8000-0000000000ac".to_string(), "c".to_string());
        let t0 = 1_792_000_000_000.0;
        let found = Found { pids: vec![1], started: t0, left: Some(t0 + 5_000.0), path: "gone.output".into(), ..Default::default() };
        let left = sample(HashMap::from([(key, found)]), t0 + 10_000.0);
        assert_eq!((left[0].left, left[0].last_output), (true, t0 + 5_000.0));
    }

    // a command of this very test: a child that holds such a file open and listens on a port
    #[cfg(not(windows))]
    #[test]
    fn a_running_command_is_found_with_its_port() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let session = "00000000-0000-4000-8000-000000000001";
        let dir = std::env::temp_dir().join(format!("claude-test/p/{session}/tasks"));
        std::fs::create_dir_all(&dir).expect("dir");
        let output = dir.join("t1.output");
        let file = std::fs::File::create(&output).expect("file");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let port = listener.local_addr().expect("addr").port();
        let found = commands(crate::util::now_ms());
        let me = found.iter().find(|c| c.session == session && c.id == "t1").expect("found");
        assert!(me.ports.contains(&port), "{:?} lacks {port}", me.ports);
        assert!(me.memory > 0);
        drop((file, listener));
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("claude-test"));
    }

    // a command of this test: a child that holds such a file open and listens
    // on a port, telling the port in the file (this process, the one asking,
    // is never among the holders on Windows)
    #[cfg(windows)]
    #[test]
    fn a_running_command_is_found_with_its_port() {
        let _turn = LOOKING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut test = crate::procs_win::Fixture::new("port");
        let session = "00000000-0000-4000-8000-000000000001";
        let output = test.tasks("p", session).join("t1.output");
        let listen = "$l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $l.Start(); \
                      [Console]::Out.WriteLine($l.LocalEndpoint.Port); [Console]::Out.Flush(); Start-Sleep 30";
        let child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", listen])
            .stdout(std::fs::File::create(&output).expect("file"))
            .spawn()
            .expect("child");
        test.children.push(child);
        let port = (0..400)
            .find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                std::fs::read_to_string(&output).ok()?.trim().parse::<u16>().ok()
            })
            .expect("the child's port");
        let found = sample(test.look(), crate::util::now_ms());
        let it = found.iter().find(|c| c.session == session && c.id == "t1").expect("found");
        assert!(it.ports.contains(&port), "{:?} lacks {port}", it.ports);
        assert!(it.memory > 0);
        assert!(!it.left, "found by its name: not left");
    }
}
