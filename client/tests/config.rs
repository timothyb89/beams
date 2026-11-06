//! Exercise the real CLI outside a checkout, with a fake tsh that makes any
//! accidental transport call observable. No global process environment edits.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct OfflineClient(PathBuf);
impl OfflineClient {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("beams-offline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let tsh = path.join("tsh");
        std::fs::write(&tsh, "#!/bin/sh\n: > \"$BEAMS_TEST_TSH_MARKER\"\nexit 91\n").unwrap();
        std::fs::set_permissions(tsh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_beams"))
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("PATH", &self.0)
            .env("BEAMS_TEST_TSH_MARKER", self.0.join("tsh-called"))
            .args(args)
            .output()
            .unwrap()
    }
    fn assert_offline(&self) {
        assert!(!self.0.join("tsh-called").exists());
        assert!(!self.0.join(".config").exists());
    }
}
impl Drop for OfflineClient {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bundled_config_commands_work_without_checkout_or_teleport() {
    let client = OfflineClient::new();
    let valid = client.run(&["config", "validate"]);
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let explained = client.run(&["config", "explain", "--json"]);
    assert!(
        explained.status.success(),
        "{}",
        String::from_utf8_lossy(&explained.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&explained.stdout).unwrap();
    assert_eq!(value["plan"]["schema_version"], 1);
    assert_eq!(value["plan"]["shell"]["program"], "/usr/bin/fish");
    assert_eq!(value["plan"]["artifacts"].as_array().unwrap().len(), 2);
    client.assert_offline();
}

#[test]
fn invalid_explicit_profile_returns_failure_without_fallback_or_side_effects() {
    let client = OfflineClient::new();
    std::fs::write(
        client.0.join("invalid.toml"),
        "schema_version = 1\nunknown = true",
    )
    .unwrap();
    let result = client.run(&["config", "validate", "--profile", "invalid.toml"]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("invalid.toml") && error.contains("unknown field"));
    assert!(result.stdout.is_empty());
    client.assert_offline();
}

#[test]
fn packing_is_offline_and_post_is_an_explicit_later_operation() {
    let client = OfflineClient::new();
    let packed = client.run(&["config", "pack", "--output", "launch"]);
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    assert!(client.0.join("launch/pre/bundle.tar.gz").is_file());
    assert!(!client.0.join("launch/post").exists());
    let post = client.run(&["config", "pack-post", "--launch-dir", "launch"]);
    assert!(
        post.status.success(),
        "{}",
        String::from_utf8_lossy(&post.stderr)
    );
    assert!(client.0.join("launch/post/bundle.tar.gz").is_file());
    let retry = client.run(&["config", "pack-post", "--launch-dir", "launch"]);
    assert!(retry.status.success());
    client.assert_offline();
}
