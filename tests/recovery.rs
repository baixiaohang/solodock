use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use solodock::{
    app_store::{AppStore, releases::ReleaseTrigger},
    db::Database,
    domain::{
        DesiredState, DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy,
        ManagedFileContent, ManagedFileInput, PublicFileContent, SecretOperation, normalize_draft,
    },
    registry::{Platform, ResolvedImage},
    security::secret::SecretValue,
    webhook::WebhookStore,
};
use tempfile::tempdir;
use uuid::Uuid;

#[tokio::test]
async fn deleted_database_rebuilds_index_without_fabricating_audit() {
    let root = tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let apps = root.path().join("apps");
    fs::create_dir(&apps).unwrap();
    fs::set_permissions(&apps, fs::Permissions::from_mode(0o700)).unwrap();
    let key = vec![9_u8; 32];
    let store = AppStore::initialize_verified(apps, key.clone()).unwrap();
    let draft = normalize_draft(
        DraftInput {
            display_name: "Example".into(),
            discovery_image_ref: "registry.example/image:stable".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: EnvironmentInput::default(),
            files: vec![],
            ports: vec![],
            volumes: vec![],
            binds: vec![],
            owned_default_network: true,
            networks: vec![],
            health: HealthPolicy::default(),
        },
        &ExistingSecrets::default(),
        &key,
        &[],
    )
    .unwrap();
    let app_id = Uuid::new_v4();
    let revision = Uuid::new_v4();
    let release_id = Uuid::new_v4();
    let metadata = store
        .create_app(
            app_id,
            "example",
            revision,
            Uuid::new_v4(),
            &draft,
            time::OffsetDateTime::now_utc(),
        )
        .unwrap();
    let digest = format!("sha256:{}", "b".repeat(64));
    store
        .publish_v2_release(
            &metadata,
            release_id,
            &ResolvedImage {
                source_image_ref: "registry.example/image:stable".into(),
                logical_registry: "registry.example".into(),
                repository: "image".into(),
                source_tag: "stable".into(),
                source_descriptor_digest: digest.clone(),
                index_digest: None,
                manifest_digest: digest.clone(),
                runnable_image_ref: format!("registry.example/image@{digest}"),
                platform: Platform::canonical("linux", "amd64", None).unwrap(),
                local_image_id: digest.clone(),
            },
            ReleaseTrigger::Manual,
            None,
        )
        .unwrap();
    solodock::app_store::atomic::AtomicWriter::switch_release_link(
        &store.app_directory(app_id),
        "active",
        release_id,
    )
    .unwrap();
    let report = store.scan().unwrap();
    let database_path = root.path().join("state.sqlite3");
    let database = Database::open(&database_path).await.unwrap();
    database.refresh_app_index(&report).await.unwrap();
    database.close().await;
    fs::remove_file(&database_path).unwrap();

    let rebuilt = Database::open(&database_path).await.unwrap();
    rebuilt.refresh_app_index(&report).await.unwrap();
    let indexed = rebuilt
        .indexed_active(&app_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(indexed.0.as_deref(), Some(release_id.to_string().as_str()));
    assert_eq!(
        indexed.1.as_deref(),
        Some(format!("registry.example/image@{digest}").as_str())
    );
    assert_eq!(rebuilt.audit_count().await.unwrap(), 0);
    assert!(!rebuilt.has_admin().await.unwrap());
}

#[test]
fn offline_backup_and_restore_preserve_verified_active_and_pending_links() {
    let fixture = tempdir().unwrap();
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let package_root = fixture.path().join("root");
    let state = package_root.join("var/lib/solodock");
    let config_directory = package_root.join("etc/solodock");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&config_directory).unwrap();
    for directory in [&package_root, &state, &config_directory] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let key = vec![7_u8; 32];
    let secrets = state.join("secrets");
    fs::create_dir(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(secrets.join("idempotency.key"), &key).unwrap();
    fs::set_permissions(
        secrets.join("idempotency.key"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let store = AppStore::initialize_managed(state.join("apps"), key.clone(), vec![]).unwrap();
    let draft = normalize_draft(
        DraftInput {
            display_name: "Backup fixture".into(),
            discovery_image_ref: "registry.example/app:stable".into(),
            credential_ref: None,
            auto_deploy_enabled: false,
            auto_deploy_acknowledged: false,
            poll_interval_seconds: 300,
            environment: EnvironmentInput::default(),
            files: vec![
                ManagedFileInput {
                    logical_name: "public-config".into(),
                    target_path: "/app/config".into(),
                    sensitive: false,
                    readonly: true,
                    content: ManagedFileContent::Public(PublicFileContent {
                        content: "include=/app/secret".into(),
                    }),
                },
                ManagedFileInput {
                    logical_name: "secret-config".into(),
                    target_path: "/app/secret".into(),
                    sensitive: true,
                    readonly: true,
                    content: ManagedFileContent::Secret(SecretOperation::Replace {
                        value: "restore-secret-canary".into(),
                    }),
                },
            ],
            ports: vec![],
            volumes: vec![],
            binds: vec![],
            owned_default_network: true,
            networks: vec![],
            health: HealthPolicy::default(),
        },
        &ExistingSecrets::default(),
        &key,
        &[],
    )
    .unwrap();
    let app_id = Uuid::new_v4();
    let revision = Uuid::new_v4();
    let now = time::OffsetDateTime::now_utc();
    let metadata = store
        .create_app(app_id, "backup", revision, Uuid::new_v4(), &draft, now)
        .unwrap();
    let resolved = |byte: char| {
        let digest = format!("sha256:{}", byte.to_string().repeat(64));
        ResolvedImage {
            source_image_ref: "registry.example/app:stable".into(),
            logical_registry: "registry.example".into(),
            repository: "app".into(),
            source_tag: "stable".into(),
            source_descriptor_digest: digest.clone(),
            index_digest: None,
            manifest_digest: digest.clone(),
            runnable_image_ref: format!("registry.example/app@{digest}"),
            platform: Platform::canonical("linux", "amd64", None).unwrap(),
            local_image_id: format!("sha256:{}", "f".repeat(64)),
        }
    };
    let active = Uuid::new_v4();
    store
        .publish_v2_release(
            &metadata,
            active,
            &resolved('a'),
            ReleaseTrigger::Manual,
            None,
        )
        .unwrap();
    store.set_pending(app_id, active).unwrap();
    store
        .finalize_active(
            app_id,
            None,
            active,
            DesiredState::Running,
            Uuid::new_v4(),
            now,
        )
        .unwrap();
    let pending = Uuid::new_v4();
    let metadata = store.read_metadata(app_id).unwrap();
    store
        .publish_v2_release(
            &metadata,
            pending,
            &resolved('b'),
            ReleaseTrigger::Poll,
            None,
        )
        .unwrap();
    store.set_pending(app_id, pending).unwrap();
    let report = store.scan_read_only().unwrap();
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let webhook_secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned());
    let webhook_store = WebhookStore::new(store.clone(), key.clone());
    webhook_store
        .configure(app_id, None, Uuid::new_v4(), &webhook_secret)
        .unwrap();

    let config = config_directory.join("config.toml");
    fs::write(
        &config,
        format!(
            "schema_version=1\nlisten_address='127.0.0.1:8080'\npublic_origin='https://solodock.example.invalid'\nwebhook_public_origin='https://hooks.example.invalid'\nstate_directory='{}'\nruntime_directory='/run/solodock'\nallowed_bind_roots=[]\n",
            state.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let archive = fixture.path().join("backup.tar");
    let backup = Command::new(manifest.join("packaging/solodock-backup"))
        .current_dir(manifest)
        .args(["--root", package_root.to_str().unwrap(), "--output"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "{}",
        String::from_utf8_lossy(&backup.stderr)
    );
    let restored = fixture.path().join("restored");
    let restore = Command::new(manifest.join("packaging/solodock-restore"))
        .current_dir(manifest)
        .args(["--archive"])
        .arg(&archive)
        .args(["--checksum"])
        .arg(format!("{}.sha256", archive.display()))
        .args(["--output"])
        .arg(&restored)
        .args(["--validator", env!("CARGO_BIN_EXE_solodock")])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let restored_app = restored
        .join("var/lib/solodock/apps")
        .join(app_id.to_string());
    for path in [
        restored_app
            .join("config-revisions")
            .join(revision.to_string())
            .join("files/public/public-config"),
        restored_app
            .join("config-revisions")
            .join(revision.to_string())
            .join("files/secret/secret-config"),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }
    assert_eq!(
        fs::read_link(restored_app.join("active")).unwrap(),
        std::path::PathBuf::from(format!("releases/{active}"))
    );
    assert_eq!(
        fs::read_link(restored_app.join("pending")).unwrap(),
        std::path::PathBuf::from(format!("releases/{pending}"))
    );
    let restored_store =
        AppStore::initialize_managed(restored.join("var/lib/solodock/apps"), key.clone(), vec![])
            .unwrap();
    let restored_webhook = WebhookStore::new(restored_store, key);
    assert!(
        restored_webhook
            .load_current(app_id)
            .unwrap()
            .secret
            .constant_time_eq(webhook_secret.expose())
    );
}
