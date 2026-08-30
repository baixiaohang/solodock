use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

#[cfg(feature = "docker-e2e")]
use std::collections::VecDeque;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    docker::models::{DockerReadApi, ImageRecord},
    registry::{LoadedCredential, ResolvedImage},
};

const DOCKER: &str = "/usr/bin/docker";
const OUTPUT_LIMIT: usize = 64 * 1024;

#[async_trait]
pub trait ImagePuller: Send + Sync {
    async fn pull(
        &self,
        deployment_id: Uuid,
        resolved: &ResolvedImage,
        credential: Option<&LoadedCredential>,
        redaction: Vec<Vec<u8>>,
    ) -> Result<(), PullError>;
}

#[derive(Clone)]
pub struct FixedImagePuller {
    runtime_directory: PathBuf,
    state_directory: PathBuf,
    docker: Arc<dyn DockerReadApi>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    docker_host: Option<String>,
    #[cfg(feature = "docker-e2e")]
    pressure_root: Option<PathBuf>,
    #[cfg(feature = "docker-e2e")]
    test_gate: Option<TestPullGate>,
}

#[cfg(feature = "docker-e2e")]
#[derive(Clone, Copy, Debug)]
pub enum TestPullAction {
    Continue,
    Pause,
    Interrupt,
}

#[cfg(feature = "docker-e2e")]
#[derive(Clone)]
pub struct TestPullGate {
    actions: Arc<std::sync::Mutex<VecDeque<TestPullAction>>>,
    reached: Arc<tokio::sync::Semaphore>,
    resume: Arc<tokio::sync::Semaphore>,
}

#[cfg(feature = "docker-e2e")]
impl TestPullGate {
    pub fn new(actions: impl IntoIterator<Item = TestPullAction>) -> Self {
        Self {
            actions: Arc::new(std::sync::Mutex::new(actions.into_iter().collect())),
            reached: Arc::new(tokio::sync::Semaphore::new(0)),
            resume: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    pub async fn wait_until_reached(&self) {
        self.reached
            .acquire()
            .await
            .expect("test pull gate remains open")
            .forget();
    }

    pub fn resume(&self) {
        self.resume.add_permits(1);
    }

    pub fn push(&self, action: TestPullAction) {
        self.actions
            .lock()
            .expect("test pull gate lock is not poisoned")
            .push_back(action);
    }

    async fn enter(&self, shutdown: &CancellationToken) -> Result<(), PullError> {
        let action = self
            .actions
            .lock()
            .expect("test pull gate lock is not poisoned")
            .pop_front();
        let Some(action) = action else {
            return Ok(());
        };
        match action {
            TestPullAction::Continue => Ok(()),
            TestPullAction::Interrupt => Err(PullError::Interrupted),
            TestPullAction::Pause => {
                self.reached.add_permits(1);
                tokio::select! {
                    () = shutdown.cancelled() => Err(PullError::Interrupted),
                    permit = self.resume.acquire() => {
                        permit.expect("test pull resume gate remains open").forget();
                        Ok(())
                    }
                }
            }
        }
    }
}

impl FixedImagePuller {
    pub fn new(
        state_directory: PathBuf,
        runtime_directory: PathBuf,
        docker: Arc<dyn DockerReadApi>,
        shutdown: CancellationToken,
        tasks: TaskTracker,
    ) -> Result<Self, PullError> {
        let root = runtime_directory.join("docker-config");
        crate::security::permissions::ensure_private_directory(&root)
            .map_err(|_| PullError::UnsafePath)?;
        Ok(Self {
            runtime_directory: root,
            state_directory,
            docker,
            shutdown,
            tasks,
            docker_host: None,
            #[cfg(feature = "docker-e2e")]
            pressure_root: None,
            #[cfg(feature = "docker-e2e")]
            test_gate: None,
        })
    }

    #[cfg(feature = "docker-e2e")]
    pub fn with_test_host(mut self, host: String) -> Self {
        self.docker_host = Some(host);
        self
    }

    #[cfg(feature = "docker-e2e")]
    pub fn with_test_pressure_root(mut self, root: PathBuf) -> Self {
        self.pressure_root = Some(root);
        self
    }

    #[cfg(feature = "docker-e2e")]
    pub fn with_test_gate(mut self, gate: TestPullGate) -> Self {
        self.test_gate = Some(gate);
        self
    }

    pub fn cleanup_stale(&self) -> Result<(), PullError> {
        for entry in fs::read_dir(&self.runtime_directory).map_err(|_| PullError::UnsafePath)? {
            let entry = entry.map_err(|_| PullError::UnsafePath)?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.parse::<Uuid>().ok())
            else {
                continue;
            };
            if entry.file_name() != std::ffi::OsStr::new(&id.to_string()) {
                continue;
            }
            crate::security::permissions::check_private_tree(
                &self.runtime_directory,
                &entry.path(),
                true,
            )
            .map_err(|_| PullError::UnsafePath)?;
            fs::remove_dir_all(entry.path()).map_err(|_| PullError::UnsafePath)?;
        }
        crate::app_store::sync_directory(&self.runtime_directory).map_err(|_| PullError::UnsafePath)
    }

    fn create_config(
        &self,
        deployment_id: Uuid,
        resolved: &ResolvedImage,
        credential: Option<&LoadedCredential>,
    ) -> Result<PathBuf, PullError> {
        self.create_config_with_hook(deployment_id, resolved, credential, |_, _| Ok(()))
    }

    fn create_config_with_hook(
        &self,
        deployment_id: Uuid,
        resolved: &ResolvedImage,
        credential: Option<&LoadedCredential>,
        hook: impl Fn(PullConfigStage, &Path) -> std::io::Result<()>,
    ) -> Result<PathBuf, PullError> {
        let directory = self.runtime_directory.join(deployment_id.to_string());
        match fs::symlink_metadata(&directory) {
            Ok(_) => self
                .remove_config(&directory)
                .map_err(|_| PullError::CleanupFailed)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PullError::CleanupFailed),
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| PullError::UnsafePath)?;
        let secret_bearing = credential.is_some();
        let result = (|| {
            let key = if resolved.logical_registry == "docker.io" {
                "https://index.docker.io/v1/"
            } else {
                &resolved.logical_registry
            };
            let key = serde_json::to_vec(key).map_err(|_| PullError::UnsafePath)?;
            let mut bytes = Zeroizing::new(Vec::with_capacity(256));
            bytes.extend_from_slice(b"{\"auths\":{");
            if let Some(value) = credential {
                let mut joined = Zeroizing::new(String::with_capacity(
                    value.metadata.username.len() + value.secret.expose().len() + 1,
                ));
                joined.push_str(&value.metadata.username);
                joined.push(':');
                joined.push_str(value.secret.expose());
                let auth = Zeroizing::new(STANDARD.encode(joined.as_bytes()));
                bytes.extend_from_slice(&key);
                bytes.extend_from_slice(b":{\"auth\":\"");
                bytes.extend_from_slice(auth.as_bytes());
                bytes.extend_from_slice(b"\"}");
            }
            bytes.extend_from_slice(b"}}");
            hook(PullConfigStage::BeforeOpen, &directory).map_err(|_| PullError::UnsafePath)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join("config.json"))
                .map_err(|_| PullError::UnsafePath)?;
            hook(PullConfigStage::AfterOpen, &directory).map_err(|_| PullError::UnsafePath)?;
            file.write_all(&bytes).map_err(|_| PullError::UnsafePath)?;
            hook(PullConfigStage::AfterWrite, &directory).map_err(|_| PullError::UnsafePath)?;
            file.sync_all().map_err(|_| PullError::UnsafePath)?;
            hook(PullConfigStage::AfterFileSync, &directory).map_err(|_| PullError::UnsafePath)?;
            crate::app_store::sync_directory(&directory).map_err(|_| PullError::UnsafePath)?;
            Ok(directory.clone())
        })();
        if result.is_err() {
            let cleanup = self.remove_config(&directory);
            if secret_bearing && cleanup.is_err() {
                return Err(PullError::CleanupFailed);
            }
        }
        result
    }

    fn remove_config(&self, directory: &Path) -> Result<(), PullError> {
        crate::security::permissions::check_private_tree(&self.runtime_directory, directory, true)
            .map_err(|_| PullError::UnsafePath)?;
        fs::remove_dir_all(directory).map_err(|_| PullError::UnsafePath)?;
        crate::app_store::sync_directory(&self.runtime_directory).map_err(|_| PullError::UnsafePath)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PullConfigStage {
    BeforeOpen,
    AfterOpen,
    AfterWrite,
    AfterFileSync,
}

#[async_trait]
impl ImagePuller for FixedImagePuller {
    async fn pull(
        &self,
        deployment_id: Uuid,
        resolved: &ResolvedImage,
        credential: Option<&LoadedCredential>,
        redaction: Vec<Vec<u8>>,
    ) -> Result<(), PullError> {
        let mut redaction = Zeroizing::new(redaction);
        #[cfg(feature = "docker-e2e")]
        if let Some(gate) = &self.test_gate {
            gate.enter(&self.shutdown).await?;
        }
        if let Some(credential) = credential {
            let mut joined = Zeroizing::new(String::with_capacity(
                credential.metadata.username.len() + credential.secret.expose().len() + 1,
            ));
            joined.push_str(&credential.metadata.username);
            joined.push(':');
            joined.push_str(credential.secret.expose());
            redaction.push(joined.as_bytes().to_vec());
            let encoded = Zeroizing::new(STANDARD.encode(joined.as_bytes()));
            redaction.push(encoded.as_bytes().to_vec());
        }
        let probe = self
            .docker
            .probe()
            .await
            .map_err(|_| PullError::Unavailable)?;
        let docker_root = probe.docker_root_directory.ok_or(PullError::Unavailable)?;
        #[cfg(feature = "docker-e2e")]
        let pressure_root = self
            .pressure_root
            .as_deref()
            .unwrap_or(Path::new(&docker_root));
        #[cfg(not(feature = "docker-e2e"))]
        let pressure_root = Path::new(&docker_root);
        check_disk_pressure(&self.state_directory, 64 * 1024 * 1024)?;
        check_disk_pressure(pressure_root, 256 * 1024 * 1024)?;
        check_memory_pressure()?;
        let directory = self.create_config(deployment_id, resolved, credential)?;
        let result = async {
            let mut command = Command::new(DOCKER);
            command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", "/nonexistent")
                .arg("--config").arg(&directory).arg("image").arg("pull").arg(&resolved.runnable_image_ref)
                .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
            if let Some(host) = &self.docker_host { command.env("DOCKER_HOST", host); }
            let mut child = command.spawn().map_err(|_| PullError::Unavailable)?;
            let stdout = child.stdout.take().ok_or(PullError::Unavailable)?;
            let stderr = child.stderr.take().ok_or(PullError::Unavailable)?;
            let stdout_task = self.tasks.spawn(drain(stdout));
            let stderr_task = self.tasks.spawn(drain(stderr));
            let status = tokio::select! {
                () = self.shutdown.cancelled() => { let _ = child.kill().await; return Err(PullError::Interrupted); }
                value = tokio::time::timeout(Duration::from_secs(600), child.wait()) => match value {
                    Ok(Ok(status)) => status,
                    Ok(Err(_)) => return Err(PullError::Unavailable),
                    Err(_) => { let _ = child.kill().await; return Err(PullError::Interrupted); }
                }
            };
            let stdout = stdout_task.await.map_err(|_| PullError::Unavailable)?.map_err(|_| PullError::Unavailable)?;
            let stderr = stderr_task.await.map_err(|_| PullError::Unavailable)?.map_err(|_| PullError::Unavailable)?;
            let output = Zeroizing::new([stdout.as_slice(), stderr.as_slice()].concat());
            let secret_leaked = redaction.iter().filter(|v| !v.is_empty()).any(|value| output.windows(value.len()).any(|part| part == value));
            let command_result = if secret_leaked {
                Err(PullError::OutputUnsafe)
            } else if !status.success() {
                Err(classify(&output))
            } else {
                Ok(())
            };
            command_result?;
            let image = self.docker.inspect_image(&resolved.runnable_image_ref).await.map_err(|_| PullError::VerificationFailed)?;
            if !image_matches_resolved(&image, resolved) { return Err(PullError::VerificationFailed); }
            Ok(())
        }.await;
        let cleanup = self
            .remove_config(&directory)
            .map_err(|_| PullError::CleanupFailed);
        match cleanup {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    }
}

fn check_disk_pressure(root: &Path, minimum: u128) -> Result<(), PullError> {
    let path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
        .map_err(|_| PullError::DiskPressure)?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(PullError::DiskPressure);
    }
    let stats = unsafe { stats.assume_init() };
    let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    if available < minimum {
        return Err(PullError::DiskPressure);
    }
    Ok(())
}

fn check_memory_pressure() -> Result<(), PullError> {
    let memory = fs::read_to_string("/proc/meminfo").map_err(|_| PullError::MemoryPressure)?;
    let available_kib = memory
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or(PullError::MemoryPressure)?;
    if available_kib < 128 * 1024 {
        return Err(PullError::MemoryPressure);
    }
    Ok(())
}

fn image_matches_resolved(image: &ImageRecord, resolved: &ResolvedImage) -> bool {
    let Ok(actual_platform) = crate::registry::Platform::canonical(
        &image.os,
        &image.architecture,
        image.variant.as_deref(),
    ) else {
        return false;
    };
    image.id == resolved.local_image_id
        && actual_platform == resolved.platform
        && image
            .repo_digests
            .iter()
            .any(|value| repo_digest_matches(value, resolved))
}

fn repo_digest_matches(value: &str, resolved: &ResolvedImage) -> bool {
    let Some((repository, digest)) = value.rsplit_once('@') else {
        return false;
    };
    // Classic image stores expose the pulled manifest digest here. Docker's
    // containerd image store can instead synthesize RepoDigests from the local
    // image target, which is the already verified config digest.
    if digest != resolved.manifest_digest && digest != resolved.local_image_id {
        return false;
    }
    let mut parts = repository.split('/');
    let first = parts.next().unwrap_or_default();
    let (registry, mut path) = if first.contains('.') || first.contains(':') || first == "localhost"
    {
        (first, parts.collect::<Vec<_>>().join("/"))
    } else {
        ("docker.io", repository.to_owned())
    };
    let registry = if matches!(registry, "index.docker.io" | "registry-1.docker.io") {
        "docker.io"
    } else {
        registry
    };
    if registry == "docker.io" && !path.contains('/') {
        path = format!("library/{path}");
    }
    registry == resolved.logical_registry && path == resolved.repository
}

fn classify(value: &[u8]) -> PullError {
    let text = String::from_utf8_lossy(value).to_ascii_lowercase();
    if text.contains("unauthorized") || text.contains("authentication required") {
        PullError::CredentialInvalid
    } else if text.contains("no space left") {
        PullError::DiskPressure
    } else if text.contains("permission denied") {
        PullError::PermissionDenied
    } else {
        PullError::Unavailable
    }
}

async fn drain(
    mut reader: impl tokio::io::AsyncRead + Unpin,
) -> std::io::Result<Zeroizing<Vec<u8>>> {
    let mut kept = Zeroizing::new(Vec::new());
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let left = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(left)]);
    }
    Ok(kept)
}

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("Docker pull unavailable")]
    Unavailable,
    #[error("Docker pull permission denied")]
    PermissionDenied,
    #[error("registry credential invalid")]
    CredentialInvalid,
    #[error("insufficient disk pressure margin")]
    DiskPressure,
    #[error("insufficient memory pressure margin")]
    MemoryPressure,
    #[error("image verification failed")]
    VerificationFailed,
    #[error("Docker pull was interrupted")]
    Interrupted,
    #[error("Docker output contained protected material")]
    OutputUnsafe,
    #[error("unsafe Docker credential path")]
    UnsafePath,
    #[error("Docker credential cleanup failed")]
    CleanupFailed,
}

impl PullError {
    pub const fn public_code(&self) -> &'static str {
        match self {
            Self::Unavailable => "DOCKER_PULL_UNAVAILABLE",
            Self::PermissionDenied => "DOCKER_PERMISSION_DENIED",
            Self::CredentialInvalid => "REGISTRY_CREDENTIAL_INVALID",
            Self::DiskPressure | Self::MemoryPressure => "RESOURCE_PRESSURE",
            Self::VerificationFailed => "IMAGE_VERIFICATION_FAILED",
            Self::Interrupted => "DEPLOYMENT_INTERRUPTED",
            Self::OutputUnsafe => "SECRET_OUTPUT_REJECTED",
            Self::UnsafePath => "RUNTIME_PATH_UNSAFE",
            Self::CleanupFailed => "CREDENTIAL_CLEANUP_FAILED",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::{
        docker::client::BollardReadClient,
        registry::{CredentialMetadata, Platform},
        security::secret::SecretValue,
    };

    fn fixture() -> (
        tempfile::TempDir,
        FixedImagePuller,
        ResolvedImage,
        LoadedCredential,
    ) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        for path in [&state, &runtime] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let puller = FixedImagePuller::new(
            state,
            runtime,
            Arc::new(BollardReadClient::production()),
            CancellationToken::new(),
            TaskTracker::new(),
        )
        .unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        let resolved = ResolvedImage {
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: digest.clone(),
            index_digest: None,
            manifest_digest: digest.clone(),
            runnable_image_ref: format!("registry.example/app@{digest}"),
            platform: Platform::canonical("linux", "amd64", None).unwrap(),
            local_image_id: format!("sha256:{}", "b".repeat(64)),
        };
        let now = time::OffsetDateTime::now_utc();
        let credential = LoadedCredential {
            metadata: CredentialMetadata {
                schema_version: 1,
                id: Uuid::new_v4(),
                revision: Uuid::new_v4(),
                registry: "registry.example".into(),
                username: "fixture".into(),
                secret_revision: Uuid::new_v4(),
                last_operation_id: Uuid::new_v4(),
                created_at: now,
                rotated_at: now,
                integrity_hmac: String::new(),
            },
            secret: SecretValue::new("pull-config-canary".into()),
        };
        (root, puller, resolved, credential)
    }

    #[test]
    fn credential_config_failpoints_remove_every_partial_file() {
        let (_root, puller, resolved, credential) = fixture();
        for stage in [
            PullConfigStage::BeforeOpen,
            PullConfigStage::AfterOpen,
            PullConfigStage::AfterWrite,
            PullConfigStage::AfterFileSync,
        ] {
            let operation = Uuid::new_v4();
            let error = puller
                .create_config_with_hook(operation, &resolved, Some(&credential), |current, _| {
                    if current == stage {
                        Err(std::io::Error::other("injected config failure"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert!(matches!(error, PullError::UnsafePath));
            assert!(
                !puller
                    .runtime_directory
                    .join(operation.to_string())
                    .exists()
            );
        }
    }

    #[test]
    fn image_verification_accepts_classic_manifest_repo_digest() {
        let (_root, _puller, resolved, _credential) = fixture();
        let image = ImageRecord {
            id: resolved.local_image_id.clone(),
            repo_digests: vec![format!("registry.example/app@{}", resolved.manifest_digest)],
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        };

        assert!(image_matches_resolved(&image, &resolved));
    }

    #[test]
    fn image_verification_accepts_containerd_config_repo_digest() {
        let (_root, _puller, mut resolved, _credential) = fixture();
        resolved.source_image_ref = "docker.io/library/postgres:16-alpine".into();
        resolved.logical_registry = "docker.io".into();
        resolved.repository = "library/postgres".into();
        resolved.runnable_image_ref =
            format!("docker.io/library/postgres@{}", resolved.manifest_digest);
        let image = ImageRecord {
            id: resolved.local_image_id.clone(),
            repo_digests: vec![format!("postgres@{}", resolved.local_image_id)],
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        };

        assert!(image_matches_resolved(&image, &resolved));
    }

    #[test]
    fn image_verification_keeps_digest_repository_and_platform_guards() {
        let (_root, _puller, resolved, _credential) = fixture();
        let valid = ImageRecord {
            id: resolved.local_image_id.clone(),
            repo_digests: vec![format!("registry.example/app@{}", resolved.local_image_id)],
            os: "linux".into(),
            architecture: "amd64".into(),
            variant: None,
        };

        let mut wrong_id = valid.clone();
        wrong_id.id = format!("sha256:{}", "c".repeat(64));
        assert!(!image_matches_resolved(&wrong_id, &resolved));

        let mut wrong_repository = valid.clone();
        wrong_repository.repo_digests = vec![format!(
            "registry.example/other@{}",
            resolved.local_image_id
        )];
        assert!(!image_matches_resolved(&wrong_repository, &resolved));

        let mut wrong_digest = valid.clone();
        wrong_digest.repo_digests = vec![format!("registry.example/app@sha256:{}", "c".repeat(64))];
        assert!(!image_matches_resolved(&wrong_digest, &resolved));

        let mut tag_only = valid.clone();
        tag_only.repo_digests = vec!["registry.example/app:stable".into()];
        assert!(!image_matches_resolved(&tag_only, &resolved));

        let mut wrong_platform = valid;
        wrong_platform.architecture = "arm64".into();
        assert!(!image_matches_resolved(&wrong_platform, &resolved));
    }

    #[test]
    fn credential_config_cleanup_failure_has_security_priority() {
        let (_root, puller, resolved, credential) = fixture();
        let runtime = puller.runtime_directory.clone();
        let operation = Uuid::new_v4();
        let error = puller
            .create_config_with_hook(operation, &resolved, Some(&credential), |stage, _| {
                if stage == PullConfigStage::AfterWrite {
                    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o000))?;
                    return Err(std::io::Error::other("injected write failure"));
                }
                Ok(())
            })
            .unwrap_err();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(error, PullError::CleanupFailed));
        puller
            .remove_config(&runtime.join(operation.to_string()))
            .unwrap();
    }
}
