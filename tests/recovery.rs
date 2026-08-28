use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

use solodock::{app_store::AppStore, db::Database};
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
