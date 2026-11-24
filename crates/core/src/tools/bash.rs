use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::ToolOutcome;
use super::cap::OutputBuffer;
use super::paths::resolve;
use super::registry::BashArgs;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs one `bash -c` in its own process group, streaming lines to `progress` while collecting at
/// most [`super::OUTPUT_LIMIT`] bytes for the model.
enum End {
    Finished,
    TimedOut,
    Cancelled,
}

pub(crate) async fn run(
    cwd: &Path,
    args: BashArgs,
    progress: &mut (dyn FnMut(&str) + Send),
    cancel: &CancellationToken,
) -> ToolOutcome {
    let dir = args
        .workdir
        .as_deref()
        .map_or_else(|| cwd.to_path_buf(), |workdir| resolve(cwd, workdir));
    let timeout = timeout_for(args.timeout_seconds);

    let mut command = build_command(&dir, &args.command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ToolOutcome::error(format!("cannot start bash: {error}")),
    };
    let group = child.id().map(|pid| Pid::from_raw(pid.cast_signed()));
    let mut guard = GroupGuard(group);
    let Some(stdout) = child.stdout.take() else {
        return ToolOutcome::error("bash produced no stdout pipe");
    };

    let mut lines = BufReader::new(stdout).lines();
    let mut output = OutputBuffer::default();

    // The read future borrows `output` and `progress`; the block ends that borrow before the
    // collected output is finished and returned.
    let end = {
        let read = read_lines(&mut lines, &mut output, progress);
        tokio::pin!(read);
        tokio::select! {
            () = &mut read => End::Finished,
            () = tokio::time::sleep(timeout) => End::TimedOut,
            () = cancel.cancelled() => End::Cancelled,
        }
    };

    if !matches!(end, End::Finished) {
        if let Some(group) = group {
            let _ = killpg(group, Signal::SIGKILL);
        }
        // Whatever the command wrote before the kill is still in the pipe: read it, bounded.
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            read_lines(&mut lines, &mut output, progress),
        )
        .await;
    }
    let status = child.wait().await;
    // The command finished under its own control: children it left behind are its business, not
    // the harness's. Dropping this future instead skips this line, and the guard kills the group.
    guard.disarm();
    let output = output.finish();

    match end {
        End::TimedOut => ToolOutcome::error(with_output(
            &output,
            &format!(
                "[timed out after {}s; process group killed]",
                timeout.as_secs()
            ),
        )),
        End::Cancelled => {
            ToolOutcome::error(with_output(&output, "[cancelled; process group killed]"))
        }
        End::Finished => match status {
            Ok(status) if status.success() => ToolOutcome::ok(with_output(&output, "")),
            Ok(status) => ToolOutcome::error(with_output(
                &output,
                &format!("[exit status: {}]", status.code().unwrap_or(-1)),
            )),
            Err(error) => ToolOutcome::error(with_output(
                &output,
                &format!("[cannot collect exit status: {error}]"),
            )),
        },
    }
}

/// The command the model's text runs in. `OVERSEER_API_KEY` is removed: the model must not be
/// able to read the harness's own credential out of the environment.
fn build_command(dir: &Path, command_text: &str) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        // The shell merges stderr into the stdout pipe, so the two interleave as the command wrote
        // them; two pipes cannot preserve that order.
        .arg(format!("exec 2>&1\n{command_text}"))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Own process group, so a timeout or cancel kills the whole tree, not just bash.
        .process_group(0)
        .kill_on_drop(true);
    strip_harness_env(&mut command, harness_env_names());
    command
}

fn strip_harness_env(command: &mut Command, names: impl IntoIterator<Item = String>) {
    for name in names {
        command.env_remove(name);
    }
}

/// The harness's own configuration, which a command must not be able to read: every variable
/// starting with `OVERSEER_` that this process holds.
fn harness_env_names() -> Vec<String> {
    std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| is_harness_var(name))
        .collect()
}

fn is_harness_var(name: &str) -> bool {
    name.starts_with("OVERSEER_")
}

/// Kills the command's process group if dropped while still armed — the path taken when the caller
/// drops the tool future instead of cancelling through the token. Disarmed once the command has
/// been reaped, so children a finished command left behind survive as before.
struct GroupGuard(Option<Pid>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }
}

async fn read_lines(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    output: &mut OutputBuffer,
    progress: &mut (dyn FnMut(&str) + Send),
) {
    while let Ok(Some(line)) = lines.next_line().await {
        progress(&line);
        output.push(&line);
        output.push("\n");
    }
}

fn with_output(output: &str, note: &str) -> String {
    match (output.is_empty(), note.is_empty()) {
        (true, true) => "(no output)".to_owned(),
        (true, false) => note.to_owned(),
        (false, true) => output.to_owned(),
        (false, false) => format!("{output}\n{note}"),
    }
}

fn timeout_for(seconds: Option<u64>) -> Duration {
    Duration::from_secs(
        seconds
            .unwrap_or(DEFAULT_TIMEOUT.as_secs())
            .clamp(1, MAX_TIMEOUT.as_secs()),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        DEFAULT_TIMEOUT, MAX_TIMEOUT, build_command, is_harness_var, strip_harness_env, timeout_for,
    };

    #[test]
    fn only_harness_variables_are_stripped() {
        assert!(is_harness_var("OVERSEER_API_KEY"));
        assert!(is_harness_var("OVERSEER_CONFIG"));
        assert!(!is_harness_var("OVERSEER"));
        assert!(!is_harness_var("PATH"));
        assert!(!is_harness_var("GITHUB_TOKEN"));
    }

    #[test]
    fn the_harness_api_key_is_not_in_the_command_environment() {
        use tokio::process::Command;

        let mut probe = Command::new("bash");
        strip_harness_env(&mut probe, ["OVERSEER_API_KEY".to_owned()]);
        let removed = probe
            .as_std()
            .get_envs()
            .any(|(name, value)| name == "OVERSEER_API_KEY" && value.is_none());
        assert!(removed, "OVERSEER_API_KEY must be removed, not inherited");

        let built = build_command(Path::new("/tmp"), "true");
        let leaked = built
            .as_std()
            .get_envs()
            .any(|(name, _)| is_harness_var(&name.to_string_lossy()));
        assert!(!leaked, "no OVERSEER_ variable may survive");
    }

    #[test]
    fn default_timeout_is_two_minutes() {
        assert_eq!(timeout_for(None), DEFAULT_TIMEOUT);
    }

    #[test]
    fn requested_timeout_is_clamped_to_one_second_and_ten_minutes() {
        assert_eq!(timeout_for(Some(0)).as_secs(), 1);
        assert_eq!(timeout_for(Some(300)).as_secs(), 300);
        assert_eq!(timeout_for(Some(99_999)), MAX_TIMEOUT);
    }
}
