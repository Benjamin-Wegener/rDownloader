//! Event hooks: run your own program when a download starts, finishes or fails,
//! and optionally show a desktop notification.
//!
//! Safety rules:
//! - A hook is an argument list (`["/path/to/program", "fixed-arg"]`), started
//!   directly, **never through a shell**. Details of the download reach the program
//!   as environment variables (`RDL_*`) and two trailing arguments (job id, file),
//!   so a file name from a server cannot inject commands.
//! - Hooks run in the background with a time limit; they never delay a download.
//! - At most [`MAX_RUNNING`] hooks run at once; further events are dropped with a warning.
//! - Notification text is passed as program arguments (not spliced into a script),
//!   stripped of control characters and truncated.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::t;

const MAX_RUNNING: usize = 4;
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TEXT_CHARS: usize = 200;

/// Hook settings (`[hooks]` in the config file). An empty list disables a hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub on_start: Vec<String>,
    pub on_complete: Vec<String>,
    pub on_error: Vec<String>,
    /// Show a desktop notification when a download finishes or fails.
    pub notify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Start,
    Complete,
    Error,
}

impl HookEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Complete => "complete",
            Self::Error => "error",
        }
    }
}

/// What a hook gets to know about the download.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub job_id: uuid::Uuid,
    pub url: String,
    pub file: PathBuf,
    pub size: u64,
    pub error: Option<String>,
}

impl HookContext {
    fn filename(&self) -> String {
        self.file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Environment of a hook process.
pub fn env_vars(event: HookEvent, ctx: &HookContext) -> HashMap<String, String> {
    let mut env = HashMap::from([
        ("RDL_EVENT".to_string(), event.name().to_string()),
        ("RDL_JOB_ID".to_string(), ctx.job_id.to_string()),
        ("RDL_URL".to_string(), ctx.url.clone()),
        (
            "RDL_FILE".to_string(),
            ctx.file.to_string_lossy().into_owned(),
        ),
        ("RDL_FILENAME".to_string(), ctx.filename()),
        ("RDL_SIZE".to_string(), ctx.size.to_string()),
    ]);
    if let Some(e) = &ctx.error {
        env.insert("RDL_ERROR".to_string(), e.clone());
    }
    env
}

/// Removes control characters (terminal and notification tricks) and shortens `s`.
pub fn clean_text(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() <= MAX_TEXT_CHARS {
        return cleaned;
    }
    let mut short: String = cleaned.chars().take(MAX_TEXT_CHARS - 1).collect();
    short.push('…');
    short
}

/// Title and body of the desktop notification for `event`, or `None` for events
/// that do not notify.
pub fn notification_text(event: HookEvent, ctx: &HookContext) -> Option<(String, String)> {
    let file = clean_text(&ctx.filename());
    match event {
        HookEvent::Start => None,
        HookEvent::Complete => Some((t!("hook.notify.complete"), file)),
        HookEvent::Error => {
            let error = clean_text(ctx.error.as_deref().unwrap_or(""));
            Some((
                t!("hook.notify.error"),
                t!("hook.notify.error_body", file = file, error = error),
            ))
        }
    }
}

/// The command that shows a notification on this platform, if one is known.
/// Text is handed over as arguments, never interpolated into a script.
fn notification_command(title: &str, body: &str) -> Option<(String, Vec<String>)> {
    if cfg!(target_os = "macos") {
        let script = [
            "on run argv",
            "display notification (item 1 of argv) with title (item 2 of argv)",
            "end run",
        ];
        let mut args: Vec<String> = script
            .iter()
            .flat_map(|l| ["-e".to_string(), l.to_string()])
            .collect();
        args.push("--".to_string());
        args.push(body.to_string());
        args.push(title.to_string());
        Some(("osascript".to_string(), args))
    } else if cfg!(target_os = "linux") {
        Some((
            "notify-send".to_string(),
            vec!["--".into(), title.into(), body.into()],
        ))
    } else {
        None
    }
}

fn slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_RUNNING)))
}

/// Runs `argv` (program first) with `env`; kills it after `timeout`.
/// Returns whether it exited successfully.
pub async fn run_command(
    argv: &[String],
    env: &HashMap<String, String>,
    timeout: Duration,
) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("Hook {program} could not start: {e}");
            return false;
        }
    };
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            warn!("Hook {program} failed: {e}");
            false
        }
        Err(_) => {
            warn!(
                "Hook {program} timed out after {} s and was stopped",
                timeout.as_secs()
            );
            let _ = child.kill().await;
            false
        }
    }
}

/// Fires the hook and notification for `event` in the background; returns at once.
pub fn fire(event: HookEvent, ctx: HookContext, cfg: &HooksConfig) {
    let hook = match event {
        HookEvent::Start => &cfg.on_start,
        HookEvent::Complete => &cfg.on_complete,
        HookEvent::Error => &cfg.on_error,
    };
    let mut jobs: Vec<(Vec<String>, HashMap<String, String>)> = Vec::new();
    if !hook.is_empty() {
        let mut argv = hook.clone();
        argv.push(ctx.job_id.to_string());
        argv.push(ctx.file.to_string_lossy().into_owned());
        jobs.push((argv, env_vars(event, &ctx)));
    }
    if cfg.notify {
        if let Some((title, body)) = notification_text(event, &ctx) {
            if let Some((program, args)) = notification_command(&title, &body) {
                let mut argv = vec![program];
                argv.extend(args);
                jobs.push((argv, HashMap::new()));
            }
        }
    }
    for (argv, env) in jobs {
        let Ok(permit) = slots().clone().try_acquire_owned() else {
            warn!("Too many hooks running, skipped {} hook", event.name());
            continue;
        };
        tokio::spawn(async move {
            let ok = run_command(&argv, &env, HOOK_TIMEOUT).await;
            debug!("Hook {:?} finished, success: {ok}", argv.first());
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> HookContext {
        HookContext {
            job_id: uuid::Uuid::nil(),
            url: "http://x.test/a.bin".into(),
            file: PathBuf::from("/dl/a.bin"),
            size: 42,
            error: None,
        }
    }

    #[test]
    fn environment_describes_the_download() {
        let mut c = ctx();
        c.error = Some("boom".into());
        let env = env_vars(HookEvent::Error, &c);
        assert_eq!(env["RDL_EVENT"], "error");
        assert_eq!(env["RDL_FILENAME"], "a.bin");
        assert_eq!(env["RDL_SIZE"], "42");
        assert_eq!(env["RDL_ERROR"], "boom");
        assert!(!env_vars(HookEvent::Complete, &ctx()).contains_key("RDL_ERROR"));
    }

    #[test]
    fn text_is_cleaned_and_shortened() {
        assert_eq!(clean_text("a\u{1b}[31mb\nc\0"), "a[31mbc");
        let long = "x".repeat(500);
        let short = clean_text(&long);
        assert_eq!(short.chars().count(), MAX_TEXT_CHARS);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn notification_texts() {
        assert!(notification_text(HookEvent::Start, &ctx()).is_none());
        let (_, body) = notification_text(HookEvent::Complete, &ctx()).unwrap();
        assert_eq!(body, "a.bin");
        let mut c = ctx();
        c.error = Some("no route".into());
        let (_, body) = notification_text(HookEvent::Error, &c).unwrap();
        assert!(body.contains("a.bin") && body.contains("no route"));
    }

    #[test]
    fn hostile_file_names_stay_data_in_the_notification_command() {
        let evil = "\" & do shell script \"touch /tmp/pwned\" & \"";
        let Some((_, args)) = notification_command("T", evil) else {
            return;
        };
        // The text is its own argument; it is never part of the script lines.
        assert!(args.iter().any(|a| a == evil));
        let script_lines: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "-e")
            .map(|w| w[1].clone())
            .collect();
        assert!(script_lines.iter().all(|l| !l.contains("pwned")));
    }

    #[test]
    fn config_defaults_to_nothing() {
        let c: HooksConfig = toml::from_str("").unwrap();
        assert_eq!(c, HooksConfig::default());
        let c: HooksConfig =
            toml::from_str("on_complete = [\"/bin/echo\", \"done\"]\nnotify = true").unwrap();
        assert_eq!(c.on_complete, ["/bin/echo", "done"]);
        assert!(c.notify && c.on_start.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_receives_environment_and_runs_without_a_shell() {
        let out = std::env::temp_dir().join(format!("rdl-hook-{}", uuid::Uuid::new_v4()));
        // A file name full of shell syntax must arrive verbatim.
        let mut c = ctx();
        c.file = PathBuf::from("/dl/$(touch pwned); `id`.bin");
        let argv: Vec<String> = [
            "sh",
            "-c",
            "printf '%s|%s' \"$RDL_EVENT\" \"$RDL_FILE\" > \"$1\"",
            "sh",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([out.to_string_lossy().into_owned()])
        .collect();
        assert!(
            run_command(
                &argv,
                &env_vars(HookEvent::Complete, &c),
                Duration::from_secs(10)
            )
            .await
        );
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(written, "complete|/dl/$(touch pwned); `id`.bin");
        let _ = std::fs::remove_file(out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_hooks_are_stopped() {
        let argv = vec!["sleep".to_string(), "30".to_string()];
        let started = std::time::Instant::now();
        assert!(!run_command(&argv, &HashMap::new(), Duration::from_millis(200)).await);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn missing_program_and_empty_command_fail_quietly() {
        assert!(
            !run_command(
                &["/no/such/program".into()],
                &HashMap::new(),
                Duration::from_secs(1)
            )
            .await
        );
        assert!(!run_command(&[], &HashMap::new(), Duration::from_secs(1)).await);
    }
}
