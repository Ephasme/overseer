//! Commands a `bash` call started in the background, kept alive and read back through `proc://`.
//!
//! Each job runs in its own process group, so killing it takes its children too; its output is
//! pumped by its own threads, because an unread pipe blocks the writer it belongs to.

use crate::config;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

/// Most jobs one session keeps; a start past this evicts the oldest finished job.
const MAX_JOBS: usize = 32;

/// Longest stream one job keeps, per pipe.
const MAX_STREAM_BYTES: usize = 64 * 1024;

/// Output lines `proc://<id>` shows from the end of stdout.
const TAIL_LINES: usize = 20;

struct Job {
    id: usize,
    command: String,
    child: Child,
    group: i32,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    started: Instant,
    /// `Some("exit 0")`, `Some("killed")`: what a reader sees once the process is gone.
    status: Option<String>,
}

static JOBS: Mutex<Vec<Option<Job>>> = Mutex::new(Vec::new());

fn jobs() -> MutexGuard<'static, Vec<Option<Job>>> {
    JOBS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn stream() -> Arc<Mutex<Vec<u8>>> {
    Arc::new(Mutex::new(Vec::new()))
}

/// Reads `pipe` to its end, keeping the first `MAX_STREAM_BYTES` and marking a longer stream.
fn pump<R: Read + Send + 'static>(pipe: Option<R>, into: Arc<Mutex<Vec<u8>>>) {
    let Some(mut pipe) = pipe else { return };
    thread::spawn(move || {
        let mut scratch = [0_u8; 8 * 1024];
        let mut truncated = false;
        loop {
            match pipe.read(&mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let kept = into.lock().unwrap_or_else(PoisonError::into_inner);
                    let room = MAX_STREAM_BYTES.saturating_sub(kept.len());
                    drop(kept);
                    let keep = read.min(room);
                    if let Some(head) = scratch.get(..keep).filter(|head| !head.is_empty()) {
                        into.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(head);
                    }
                    truncated |= keep < read;
                }
            }
        }
        if truncated {
            into.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(b"\n[truncated]");
        }
    });
}

/// Starts `command` in `dir` and returns its id; the job outlives the tool call that started it.
pub(crate) fn start(command: &str, dir: &Path) -> Result<usize, String> {
    let mut jobs = jobs();
    if jobs.iter().flatten().count() >= MAX_JOBS {
        let finished = jobs.iter_mut().position(|slot| slot.as_mut().is_some_and(exited));
        match finished {
            Some(index) => {
                if let Some(mut job) = jobs.get_mut(index).and_then(Option::take) {
                    let _ = job.child.wait();
                }
            }
            None => return Err(format!("too many jobs: {MAX_JOBS} are running")),
        }
    }
    let mut built = Command::new("sh");
    built
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = built.spawn().map_err(|e| format!("sh: {e}"))?;
    let group = i32::try_from(child.id()).map_err(|e| e.to_string())?;
    let out = stream();
    let err = stream();
    pump(child.stdout.take(), Arc::clone(&out));
    pump(child.stderr.take(), Arc::clone(&err));
    let stdin = child.stdin.take();
    let id = match jobs.iter().position(Option::is_none) {
        Some(index) => index + 1,
        None => jobs.len() + 1,
    };
    let job =
        Job { id, command: command.to_string(), child, group, stdin, out, err, started: Instant::now(), status: None };
    if id > jobs.len() {
        jobs.push(Some(job));
    } else if let Some(slot) = jobs.get_mut(id - 1) {
        *slot = Some(job);
    }
    Ok(id)
}

/// Whether the job's process is gone, recording how it ended.
fn exited(job: &mut Job) -> bool {
    if job.status.is_some() {
        return true;
    }
    match job.child.try_wait() {
        Ok(Some(status)) => {
            let code = status.code().map_or_else(|| "signal".to_string(), |code| code.to_string());
            job.status = Some(format!("exit {code}"));
            true
        }
        Ok(None) => false,
        // A reaped-but-unwaitable child is as gone as one that exited.
        Err(_) => {
            job.status = Some("failed".to_string());
            true
        }
    }
}

fn state(job: &mut Job) -> String {
    if exited(job) { job.status.clone().unwrap_or_else(|| "exited".to_string()) } else { "running".to_string() }
}

fn plain(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn tail(bytes: &[u8], lines: usize) -> String {
    let text = plain(bytes);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all.get(start..).unwrap_or_default().join("\n")
}

fn unknown(target: &str) -> String {
    let ids: Vec<String> = jobs().iter().flatten().map(|job| job.id.to_string()).collect();
    let ids = if ids.is_empty() { "none".to_string() } else { ids.join(", ") };
    format!("unknown job {target}; jobs are {ids}")
}

/// What `proc://` lists: one line per job, or that there is none.
pub(crate) fn list() -> String {
    let mut jobs = jobs();
    if jobs.iter().flatten().next().is_none() {
        return "no background jobs".to_string();
    }
    let mut lines = Vec::new();
    for job in jobs.iter_mut().flatten() {
        let state = state(job);
        let command = job.command.lines().next().unwrap_or_default();
        let secs = job.started.elapsed().as_secs();
        lines.push(format!("#{} {state} {secs}s {command}", job.id));
    }
    lines.join("\n")
}

/// `proc://<id>` (its tail) or `proc://<id>/streams` (each stream whole).
pub(crate) fn read(target: &str) -> Result<String, String> {
    let (number, field) = target.split_once('/').map_or((target, None), |(id, field)| (id, Some(field)));
    let Ok(id) = number.parse::<usize>() else { return Err(unknown(target)) };
    let mut jobs = jobs();
    let index = jobs.iter().position(|slot| slot.as_ref().is_some_and(|job| job.id == id));
    let Some(index) = index else {
        // The id list is built from the same store, so the guard has to be released first.
        drop(jobs);
        return Err(unknown(number));
    };
    let Some(job) = jobs.get_mut(index).and_then(Option::as_mut) else {
        return Err(format!("unknown job {number}"));
    };
    let state = state(job);
    let secs = job.started.elapsed().as_secs();
    let out = plain(&job.out.lock().unwrap_or_else(PoisonError::into_inner));
    let err = plain(&job.err.lock().unwrap_or_else(PoisonError::into_inner));
    match field {
        None => {
            let mut text = format!("job {id} {state} · {secs}s\n{}", tail(out.as_bytes(), TAIL_LINES));
            if !err.trim().is_empty() {
                let _ = write!(text, "\nstderr:\n{}", tail(err.as_bytes(), TAIL_LINES));
            }
            Ok(text)
        }
        Some("streams") => Ok(format!("stdout:\n{out}\nstderr:\n{err}")),
        Some(_) => Err("usage: proc://<id> or proc://<id>/streams".to_string()),
    }
}

/// `proc://<id>` (write to stdin) or `proc://<id>/kill` (end it and everything it started).
pub(crate) fn write(target: &str, content: &str) -> Result<String, String> {
    let (number, field) = target.split_once('/').map_or((target, None), |(id, field)| (id, Some(field)));
    let Ok(id) = number.parse::<usize>() else { return Err(unknown(target)) };
    let mut jobs = jobs();
    let index = jobs.iter().position(|slot| slot.as_ref().is_some_and(|job| job.id == id));
    let Some(index) = index else {
        // The id list is built from the same store, so the guard has to be released first.
        drop(jobs);
        return Err(unknown(number));
    };
    let Some(job) = jobs.get_mut(index).and_then(Option::as_mut) else {
        return Err(format!("unknown job {number}"));
    };
    if let Some("kill") = field {
        let _ = Command::new("kill").arg("-9").arg(format!("-{}", job.group)).status();
        let _ = job.child.wait();
        job.status = Some("killed".to_string());
        return Ok(format!("killed job {id}"));
    }
    if field.is_some() {
        return Err("usage: proc://<id> or proc://<id>/kill".to_string());
    }
    if exited(job) {
        return Err(format!("job {id} has exited"));
    }
    let mut line = content.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    let written = line.len();
    let Some(stdin) = job.stdin.as_mut() else { return Err(format!("job {id} has no stdin")) };
    stdin.write_all(line.as_bytes()).map_err(|e| config::io_error(format!("job {id}"), &e))?;
    stdin.flush().map_err(|e| config::io_error(format!("job {id}"), &e))?;
    Ok(format!("sent {written} bytes to job {id}"))
}

/// Kills every job still running: quitting must not leave a background command behind.
pub(crate) fn kill_all() {
    let mut jobs = jobs();
    for job in jobs.iter_mut().flatten() {
        let _ = Command::new("kill").arg("-9").arg(format!("-{}", job.group)).status();
        let _ = job.child.wait();
    }
    jobs.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the whole store: it is process-global, so parallel tests would see each other.
    #[test]
    fn a_background_job_runs_is_read_fed_and_killed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list(), "no background jobs");
        let id = start("printf 'a\\nb\\n'; sleep 30", dir.path()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let streams = read(&format!("{id}/streams")).unwrap();
            if streams == "stdout:\na\nb\n\nstderr:\n" {
                break;
            }
            assert!(Instant::now() < deadline, "the pump never caught up: {streams:?}");
            thread::sleep(std::time::Duration::from_millis(20));
        }
        let listed = list();
        assert!(listed.contains(&format!("#{id} running")), "{listed}");
        assert!(read(&format!("{id}")).unwrap().starts_with(&format!("job {id} running")));
        assert_eq!(write(&format!("{id}/kill"), "").unwrap(), format!("killed job {id}"));
        assert!(read(usize::MAX.to_string().as_str()).unwrap_err().contains("unknown job"));

        let fed = start("read line; echo \"got $line\"", dir.path()).unwrap();
        assert_eq!(write(&format!("{fed}"), "hi").unwrap(), format!("sent 3 bytes to job {fed}"));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let streams = read(&format!("{fed}/streams")).unwrap();
            if streams.contains("got hi") {
                break;
            }
            assert!(Instant::now() < deadline, "the job never answered its stdin: {streams}");
            thread::sleep(std::time::Duration::from_millis(20));
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while read(&format!("{fed}")).unwrap().contains("running") {
            assert!(Instant::now() < deadline, "the job never exited");
            thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(write(&format!("{fed}"), "more").unwrap_err(), format!("job {fed} has exited"));
        kill_all();
        assert_eq!(list(), "no background jobs");
    }
}
