use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn write_config(root: &Path, listen: &str, state: &str, runtime: &str) -> PathBuf {
    let path = root.join("config.toml");
    fs::write(
        &path,
        format!(
            "schema_version=1\nlisten_address='{listen}'\npublic_origin='https://solodock.example.invalid'\nstate_directory='{state}'\nruntime_directory='{runtime}'\nallowed_bind_roots=[]\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn inspect(config: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_solodock"))
        .arg("inspect-packaged-config")
        .arg(config)
        .output()
        .unwrap()
}

#[test]
fn inspector_emits_exact_bounded_ipv4_and_ipv6_records() {
    for (listen, expected_health, expected_local) in [
        (
            "127.9.8.7:9123",
            "HEALTH_URL=http://127.9.8.7:9123/healthz",
            "LOCAL_AUTHORITY=127.9.8.7:9123",
        ),
        (
            "[::1]:9124",
            "HEALTH_URL=http://[::1]:9124/healthz",
            "LOCAL_AUTHORITY=[::1]:9124",
        ),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let config = write_config(fixture.path(), listen, "/var/lib/solodock", "/run/solodock");
        let output = inspect(&config);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines = stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "FORMAT=solodock-packaged-config-v1");
        assert_eq!(lines[1], expected_health);
        assert_eq!(lines[2], expected_local);
        assert_eq!(
            lines[3],
            "MANAGEMENT_AUTHORITY=solodock.example.invalid:443"
        );
        assert!(
            lines
                .iter()
                .all(|line| line.len() <= 255 && !line.contains('\t'))
        );
    }
}

#[test]
fn inspector_preserves_rust_accepted_ipv6_management_authority() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("config.toml");
    fs::write(
        &path,
        "schema_version=1\nlisten_address='127.0.0.1:8080'\npublic_origin='https://[::1]:8443'\nstate_directory='/var/lib/solodock'\nruntime_directory='/run/solodock'\nallowed_bind_roots=[]\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let output = inspect(&path);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .any(|line| line == "MANAGEMENT_AUTHORITY=[::1]:8443")
    );
}

#[test]
fn packaged_rejections_happen_before_managed_directory_side_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let state = fixture.path().join("state");
    let runtime = fixture.path().join("runtime");
    let config = write_config(
        fixture.path(),
        "127.0.0.1:8080",
        state.to_str().unwrap(),
        runtime.to_str().unwrap(),
    );

    for (state_value, runtime_value, expected) in [
        (
            state.to_str().unwrap(),
            "/run/solodock",
            "PackagedStateDirectory",
        ),
        (
            "/var/lib/solodock",
            runtime.to_str().unwrap(),
            "PackagedRuntimeDirectory",
        ),
    ] {
        let config = write_config(fixture.path(), "127.0.0.1:8080", state_value, runtime_value);
        let inspection = inspect(&config);
        assert!(!inspection.status.success());
        assert!(String::from_utf8_lossy(&inspection.stderr).contains(expected));
        assert!(!state.exists() && !runtime.exists());
        assert!(!state.join("state.sqlite3").exists());
    }

    for marker in ["1", "true"] {
        let output = Command::new(env!("CARGO_BIN_EXE_solodock"))
            .env("SOLODOCK_PACKAGED_LAYOUT", marker)
            .env("SOLODOCK_CONFIG_PATH", &config)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if marker == "1" {
            assert!(stderr.contains("PackagedConfigPath"));
        } else {
            assert!(stderr.contains("PackagedLayoutMarker"));
        }
        assert!(!state.exists() && !runtime.exists());
        assert!(!state.join("state.sqlite3").exists());
    }
}

#[cfg(feature = "docker-e2e")]
#[test]
fn packaged_runtime_loader_enforces_state_and_runtime_before_side_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let state = fixture.path().join("state");
    let runtime = fixture.path().join("runtime");
    for (state_value, runtime_value, expected) in [
        (
            state.to_str().unwrap(),
            "/run/solodock",
            "PackagedStateDirectory",
        ),
        (
            "/var/lib/solodock",
            runtime.to_str().unwrap(),
            "PackagedRuntimeDirectory",
        ),
    ] {
        let config = write_config(fixture.path(), "127.0.0.1:8080", state_value, runtime_value);
        let output = Command::new(env!("CARGO_BIN_EXE_solodock"))
            .env("SOLODOCK_PACKAGED_LAYOUT", "1")
            .env("SOLODOCK_CONFIG_PATH", "/etc/solodock/config.toml")
            .env("SOLODOCK_PACKAGED_CONFIG_TEST_PATH", &config)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        assert!(!state.exists() && !runtime.exists());
        assert!(!state.join("state.sqlite3").exists());
    }
}
