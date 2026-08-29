use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;
use zeroize::Zeroize;

use super::ComposeError;
use crate::docker::logs::SecretRedactor;

const DOCKER: &str = "/usr/bin/docker";
const OUTPUT_LIMIT: usize = 64 * 1024;

pub fn cleanup_temporary_directories(runtime_directory: &Path) -> Result<(), ComposeError> {
    let compose_directory = runtime_directory.join("compose");
    crate::security::permissions::ensure_private_directory(&compose_directory)
        .map_err(|_| ComposeError::UnsafePath)?;
    for entry in std::fs::read_dir(&compose_directory).map_err(|_| ComposeError::UnsafePath)? {
        let entry = entry.map_err(|_| ComposeError::UnsafePath)?;
        let name = entry.file_name();
        let Some(operation_id) = name.to_str().and_then(|value| value.parse::<Uuid>().ok()) else {
            continue;
        };
        if name.as_os_str() != std::ffi::OsStr::new(&operation_id.to_string()) {
            continue;
        }
        validate_private_tree(&entry.path())?;
        std::fs::remove_dir_all(entry.path()).map_err(|_| ComposeError::UnsafePath)?;
    }
    std::fs::File::open(compose_directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ComposeError::UnsafePath)
}

fn validate_private_tree(root: &Path) -> Result<(), ComposeError> {
    crate::security::permissions::check_private(root, true)
        .map_err(|_| ComposeError::UnsafePath)?;
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).map_err(|_| ComposeError::UnsafePath)? {
            let entry = entry.map_err(|_| ComposeError::UnsafePath)?;
            let file_type = entry.file_type().map_err(|_| ComposeError::UnsafePath)?;
            if file_type.is_symlink() {
                return Err(ComposeError::UnsafePath);
            }
            if file_type.is_dir() {
                crate::security::permissions::check_private(&entry.path(), true)
                    .map_err(|_| ComposeError::UnsafePath)?;
                pending.push(entry.path());
            } else if file_type.is_file() {
                crate::security::permissions::check_private(&entry.path(), false)
                    .map_err(|_| ComposeError::UnsafePath)?;
            } else {
                return Err(ComposeError::UnsafePath);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeAction {
    Version,
    Validate,
    Start,
    Recreate,
    Stop,
    Restart,
    Remove,
}

#[derive(Clone, Debug)]
pub struct RunContext {
    pub project_name: String,
    pub project_directory: PathBuf,
    pub compose_file: PathBuf,
    pub timeout: Duration,
    pub redaction_patterns: Vec<Vec<u8>>,
}

impl RunContext {
    pub fn capability() -> Self {
        Self {
            project_name: String::new(),
            project_directory: PathBuf::new(),
            compose_file: PathBuf::new(),
            timeout: Duration::from_secs(5),
            redaction_patterns: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComposeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[async_trait]
pub trait ComposeRunner: Send + Sync {
    async fn run(
        &self,
        action: ComposeAction,
        context: RunContext,
    ) -> Result<ComposeOutput, ComposeError>;
}

impl RunContext {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[derive(Clone)]
pub struct FixedComposeRunner {
    shutdown: CancellationToken,
    tasks: TaskTracker,
    redactor: SecretRedactor,
    docker_host: Option<String>,
}

impl FixedComposeRunner {
    pub fn new(shutdown: CancellationToken, tasks: TaskTracker, redactor: SecretRedactor) -> Self {
        Self {
            shutdown,
            tasks,
            redactor,
            docker_host: None,
        }
    }

    #[cfg(feature = "docker-e2e")]
    pub fn for_test_http(
        shutdown: CancellationToken,
        tasks: TaskTracker,
        redactor: SecretRedactor,
        docker_host: String,
    ) -> Self {
        Self {
            shutdown,
            tasks,
            redactor,
            docker_host: Some(docker_host),
        }
    }
}

#[async_trait]
impl ComposeRunner for FixedComposeRunner {
    async fn run(
        &self,
        action: ComposeAction,
        mut context: RunContext,
    ) -> Result<ComposeOutput, ComposeError> {
        let mut command = Command::new(DOCKER);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .env("COMPOSE_DISABLE_ENV_FILE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(host) = &self.docker_host {
            command.env("DOCKER_HOST", host);
        }
        for arg in argv(action, &context) {
            command.arg(arg);
        }
        let mut child = command.spawn().map_err(|_| ComposeError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(ComposeError::Unavailable)?;
        let stderr = child.stderr.take().ok_or(ComposeError::Unavailable)?;
        let stdout_task = self.tasks.spawn(drain(stdout));
        let stderr_task = self.tasks.spawn(drain(stderr));
        let status = tokio::select! {
            () = self.shutdown.cancelled() => { let _ = child.kill().await; return Err(ComposeError::Cancelled); }
            result = tokio::time::timeout(context.timeout, child.wait()) => match result {
                Ok(Ok(status)) => status,
                Ok(Err(_)) => return Err(ComposeError::Unavailable),
                Err(_) => { let _ = child.kill().await; return Err(ComposeError::Timeout); }
            }
        };
        let stdout = stdout_task
            .await
            .map_err(|_| ComposeError::Unavailable)?
            .map_err(|_| ComposeError::Unavailable)?;
        let stderr = stderr_task
            .await
            .map_err(|_| ComposeError::Unavailable)?
            .map_err(|_| ComposeError::Unavailable)?;
        // Version output is control-plane input, not user-visible diagnostic
        // output. Redacting it can turn a valid version into an incompatible
        // one when a known secret is a short digit sequence.
        let operation_redactor = self
            .redactor
            .with_additional(context.redaction_patterns.iter().cloned());
        context.redaction_patterns.zeroize();
        let output = if action == ComposeAction::Version {
            ComposeOutput {
                stdout,
                stderr: operation_redactor.redact(&stderr),
            }
        } else {
            ComposeOutput {
                stdout: operation_redactor.redact(&stdout),
                stderr: operation_redactor.redact(&stderr),
            }
        };
        if status.success() {
            Ok(output)
        } else if action == ComposeAction::Version {
            Err(classify_failure(&stderr, false))
        } else {
            Err(classify_failure(&stderr, action == ComposeAction::Validate))
        }
    }
}

fn classify_failure(stderr: &[u8], validation: bool) -> ComposeError {
    let diagnostic = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if diagnostic.contains("permission denied") || diagnostic.contains("access is denied") {
        ComposeError::PermissionDenied
    } else if diagnostic.contains("cannot connect")
        || diagnostic.contains("connection refused")
        || diagnostic.contains("daemon is not running")
        || diagnostic.contains("context canceled")
    {
        ComposeError::Unavailable
    } else if validation {
        ComposeError::ValidationFailed
    } else {
        // A lifecycle non-zero exit without a recognized daemon/permission
        // signal is deterministic for this pinned canonical project.
        ComposeError::ValidationFailed
    }
}

fn argv(action: ComposeAction, context: &RunContext) -> Vec<String> {
    if action == ComposeAction::Version {
        return vec!["compose".into(), "version".into(), "--short".into()];
    }
    let mut result = vec![
        "compose".into(),
        "--project-name".into(),
        context.project_name.clone(),
        "--project-directory".into(),
        path(context.project_directory.as_path()),
        "--env-file".into(),
        "/dev/null".into(),
        "--file".into(),
        path(context.compose_file.as_path()),
    ];
    match action {
        ComposeAction::Validate => result.extend(["config", "--quiet"].map(str::to_owned)),
        ComposeAction::Start => result.extend(["start", "app"].map(str::to_owned)),
        ComposeAction::Recreate => result.extend(
            [
                "up",
                "--detach",
                "--no-build",
                "--pull",
                "never",
                "--no-deps",
                "app",
            ]
            .map(str::to_owned),
        ),
        ComposeAction::Stop => result.extend(["stop", "--timeout", "30", "app"].map(str::to_owned)),
        ComposeAction::Restart => {
            result.extend(["restart", "--no-deps", "--timeout", "30", "app"].map(str::to_owned))
        }
        ComposeAction::Remove => {
            result.extend(["rm", "--stop", "--force", "app"].map(str::to_owned))
        }
        ComposeAction::Version => unreachable!(),
    }
    result
}

fn path(value: &Path) -> String {
    value.as_os_str().to_string_lossy().into_owned()
}

async fn drain(mut reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn actions_cannot_construct_forbidden_commands() {
        let context = RunContext {
            project_name: "solodock-test".into(),
            project_directory: "/state/app".into(),
            compose_file: "/state/app/releases/id/compose.yaml".into(),
            timeout: Duration::from_secs(60),
            redaction_patterns: Vec::new(),
        };
        for action in [
            ComposeAction::Validate,
            ComposeAction::Start,
            ComposeAction::Recreate,
            ComposeAction::Stop,
            ComposeAction::Restart,
            ComposeAction::Remove,
        ] {
            let args = argv(action, &context);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--env-file", "/dev/null"])
            );
            assert!(!args.iter().any(|arg| matches!(
                arg.as_str(),
                "down"
                    | "-v"
                    | "--volumes"
                    | "pull"
                    | "build"
                    | "exec"
                    | "run"
                    | "--remove-orphans"
            )));
        }
    }

    #[test]
    fn startup_cleanup_removes_only_canonical_private_operation_directories() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let compose = root.path().join("compose");
        fs::create_dir(&compose).unwrap();
        fs::set_permissions(&compose, fs::Permissions::from_mode(0o700)).unwrap();
        let operation = compose.join(Uuid::new_v4().to_string());
        fs::create_dir(&operation).unwrap();
        fs::set_permissions(&operation, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(operation.join("compose.yaml"), "partial").unwrap();
        fs::set_permissions(
            operation.join("compose.yaml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let unrelated = compose.join("operator-note");
        fs::write(&unrelated, "keep").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o600)).unwrap();
        cleanup_temporary_directories(root.path()).unwrap();
        assert!(!operation.exists());
        assert!(unrelated.exists());
    }
}
