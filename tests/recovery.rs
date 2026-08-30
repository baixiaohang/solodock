use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    process::Command,
};

use solodock::{
    app_store::{AppStore, releases::ReleaseTrigger},
    db::Database,
    domain::{
        DesiredState, DraftInput, EnvironmentInput, ExistingSecrets, HealthPolicy, normalize_draft,
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
    let app_id = Uuid::new_v4();
    let release_id = Uuid::new_v4();
    let app = apps.join(app_id.to_string());
    let release = app.join("releases").join(release_id.to_string());
    fs::create_dir_all(&release).unwrap();
    fs::write(app.join("app.toml"), format!("schema_version=1\nid='{app_id}'\nslug='example'\ndisplay_name='Example'\nproject_name='solodock-{}'\ncreated_at='2026-08-28T00:00:00Z'\nupdated_at='2026-08-28T00:00:00Z'\n", app_id.simple())).unwrap();
    let digest = "b".repeat(64);
    fs::write(release.join("release.toml"), format!("schema_version=1\nid='{release_id}'\napp_id='{app_id}'\nrunnable_image_ref='registry.example/image@sha256:{digest}'\ncreated_at='2026-08-28T00:00:00Z'\n")).unwrap();
    symlink(format!("releases/{release_id}"), app.join("active")).unwrap();
    for directory in [&app, &app.join("releases"), &release] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for file in [app.join("app.toml"), release.join("release.toml")] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let report = AppStore::initialize(apps).unwrap().scan().unwrap();
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
        Some(format!("registry.example/image@sha256:{digest}").as_str())
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
            slug: "backup-fixture".into(),
            display_name: "Backup fixture".into(),
            discovery_image_ref: "registry.example/app:stable".into(),
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
    let now = time::OffsetDateTime::now_utc();
    let metadata = store
        .create_app(app_id, revision, Uuid::new_v4(), &draft, now)
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
