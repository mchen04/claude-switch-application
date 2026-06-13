use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn cs() -> Command {
    let mut cmd = Command::cargo_bin("cs").expect("binary built");
    cmd.env("CS_TEST_KEYCHAIN", "1");
    // Tests should never read the real $USER's canonical Keychain entry — pin a value
    // for determinism.
    cmd.env("USER", "test-user");
    cmd
}

fn isolated() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let claude_home = dir.path().join("claude");
    let cs_home = dir.path().join("cs-home");
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::create_dir_all(&cs_home).unwrap();
    (dir, claude_home, cs_home)
}

/// Each test gets a fresh shared mock keychain by setting CS_TEST_KEYCHAIN_FIXTURE to a
/// JSON file the binary loads at startup. We pre-seed the canonical entry with a valid
/// OAuth blob.
fn fixture_path(dir: &std::path::Path, blobs: &[(&str, &str)]) -> PathBuf {
    let mut map = serde_json::Map::new();
    for (acct, blob) in blobs {
        map.insert(
            (*acct).to_string(),
            serde_json::Value::String((*blob).to_string()),
        );
    }
    let p = dir.join("keychain-fixture.json");
    std::fs::write(
        &p,
        serde_json::to_vec(&serde_json::Value::Object(map)).unwrap(),
    )
    .unwrap();
    p
}

fn fake_oauth(email: &str, expires_in_secs: i64) -> String {
    fake_oauth_tagged(email, expires_in_secs, email)
}

/// Like [`fake_oauth`] but the access/refresh tokens carry `tag` so two blobs for the
/// *same* account (same email) can be made distinguishable — modelling Claude Code
/// rotating the canonical token while the saved snapshot lags behind.
fn fake_oauth_tagged(email: &str, expires_in_secs: i64, tag: &str) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let exp = now_ms + expires_in_secs * 1000;
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": format!("tok-{tag}"),
            "refreshToken": format!("ref-{tag}"),
            "expiresAt": exp,
            "scopes": ["user:profile"],
            "subscriptionType": "max",
            "email": email
        }
    })
    .to_string()
}

/// An OAuth blob with NO `email` field — Claude Code's schema makes email optional, and cs
/// must still preserve the rotated token for these (the identity guard must not treat two
/// email-less blobs as different accounts).
fn fake_oauth_no_email(expires_in_secs: i64, tag: &str) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let exp = now_ms + expires_in_secs * 1000;
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": format!("tok-{tag}"),
            "refreshToken": format!("ref-{tag}"),
            "expiresAt": exp,
            "scopes": ["user:profile"],
            "subscriptionType": "max"
        }
    })
    .to_string()
}

#[test]
fn shows_help() {
    cs().arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude profile switching"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("master"))
        .stdout(predicate::str::contains("usage"));
}

#[test]
fn shows_version() {
    cs().arg("--version").assert().success();
}

#[test]
fn doctor_runs_in_isolated_env() {
    let (_dir, claude_home, cs_home) = isolated();
    cs().env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .arg("doctor")
        .arg("--json")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"backend\""))
        .stdout(predicate::str::contains("\"tooling\""));
}

#[test]
fn doctor_text_runs_in_isolated_env() {
    let (_dir, claude_home, cs_home) = isolated();
    cs().env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("cs doctor"))
        .stdout(predicate::str::contains("Tooling"));
}

#[test]
fn unknown_name_errors_with_not_found() {
    let (_dir, claude_home, cs_home) = isolated();
    cs().env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .arg("does-not-exist-yet")
        .assert()
        .failure()
        .stderr(predicate::str::contains("profile not found"));
}

#[test]
fn list_empty_text() {
    let (_dir, claude_home, cs_home) = isolated();
    cs().env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("no profiles saved"));
}

#[test]
fn list_empty_json_schema() {
    let (_dir, claude_home, cs_home) = isolated();
    let output = cs()
        .env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
    assert!(v.get("active").is_some(), "missing active");
    assert!(v.get("default").is_some(), "missing default");
    assert!(v.get("profiles").is_some(), "missing profiles");
    assert!(v["profiles"].as_array().unwrap().is_empty());
}

#[test]
fn status_no_active_text() {
    let (_dir, claude_home, cs_home) = isolated();
    cs().env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("no active profile"));
}

// --- switch + profile management round-trip -----------------------------------

fn phase_c_env(
    claude_home: &std::path::Path,
    cs_home: &std::path::Path,
    fixture: &std::path::Path,
) -> Command {
    let mut c = cs();
    c.env("CLAUDE_HOME", claude_home)
        .env("CS_HOME", cs_home)
        .env("CS_TEST_KEYCHAIN_FIXTURE", fixture);
    c
}

#[test]
fn save_round_trip() {
    let (dir, claude_home, cs_home) = isolated();
    let canonical_blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &canonical_blob)]);
    std::fs::write(claude_home.join("settings.json"), b"{\"theme\":\"dark\"}\n").unwrap();

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("personal"))
        .stdout(predicate::str::contains("primary@example.com"));

    assert_eq!(
        std::fs::read(cs_home.join("profiles/personal/settings.json")).unwrap(),
        b"{\"theme\":\"dark\"}\n"
    );
}

#[test]
fn save_overwrites_existing() {
    let (dir, claude_home, cs_home) = isolated();
    let first_blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &first_blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success()
        .stderr(predicate::str::contains("saved profile"));

    // Replace the canonical entry with a different account, then re-save the same profile name.
    let second_blob = fake_oauth("rotated@example.com", 7200);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &second_blob),
            ("Claude Code-credentials-personal", &first_blob),
        ],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success()
        .stderr(predicate::str::contains("overwrote profile"));

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rotated@example.com"))
        .stdout(predicate::str::contains("primary@example.com").not());
}

#[test]
fn switch_changes_canonical_and_state() {
    let (dir, claude_home, cs_home) = isolated();
    let work_blob = fake_oauth("work@example.com", 3600);
    let personal_blob = fake_oauth("personal@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &work_blob),
            ("Claude Code-credentials-personal", &personal_blob),
            ("Claude Code-credentials-work", &work_blob),
        ],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["personal"])
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "personal");

    let canonical_now: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        canonical_now["test-user"].as_str().unwrap(),
        canonical_now["Claude Code-credentials-personal"]
            .as_str()
            .unwrap()
    );
}

#[test]
fn switch_previous_toggles() {
    let (dir, claude_home, cs_home) = isolated();
    let work_blob = fake_oauth("work@example.com", 3600);
    let personal_blob = fake_oauth("personal@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &work_blob),
            ("Claude Code-credentials-personal", &personal_blob),
            ("Claude Code-credentials-work", &work_blob),
        ],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["personal"])
        .assert()
        .success();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["work"])
        .assert()
        .success();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .arg("-")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "personal");
    assert_eq!(state["previous"], "work");
}

#[test]
fn switch_away_preserves_rotated_canonical_token() {
    // Regression (logout bug): switching away from a profile must flush Claude Code's
    // freshly rotated canonical credential back into that profile's snapshot. Otherwise
    // the snapshot keeps a refresh token that rotation has invalidated server-side, and
    // switching back later reinstalls a dead token — Claude Code then logs the user out.
    let (dir, claude_home, cs_home) = isolated();
    let a_v0 = fake_oauth_tagged("a@example.com", 3600, "a-v0");
    let b_blob = fake_oauth_tagged("b@example.com", 3600, "b-v0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &a_v0),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );

    // Establish `a` as active (prior_active is None, so nothing to flush yet).
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["a"])
        .assert()
        .success();

    // Claude Code rotates the canonical access+refresh token in place while `a` runs.
    let a_v1 = fake_oauth_tagged("a@example.com", 7200, "a-v1");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &a_v1),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );

    // Switch away to `b`: the outgoing `a` snapshot must capture the rotated token.
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["b"])
        .assert()
        .success();

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        kc["Claude Code-credentials-a"].as_str().unwrap(),
        a_v1,
        "switching away should sync the rotated canonical token into profile a"
    );
    assert_eq!(kc["test-user"].as_str().unwrap(), b_blob);
}

#[test]
fn reselect_active_does_not_downgrade_canonical() {
    // Re-selecting the already-active profile must not clobber the live (rotated)
    // canonical token with the older snapshot.
    let (dir, claude_home, cs_home) = isolated();
    let v0 = fake_oauth_tagged("a@example.com", 3600, "a-v0");
    let fixture = fixture_path(
        dir.path(),
        &[("test-user", &v0), ("Claude Code-credentials-a", &v0)],
    );
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["a"])
        .assert()
        .success();

    let v1 = fake_oauth_tagged("a@example.com", 7200, "a-v1");
    let fixture = fixture_path(
        dir.path(),
        &[("test-user", &v1), ("Claude Code-credentials-a", &v0)],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["a"])
        .assert()
        .success();

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        kc["test-user"].as_str().unwrap(),
        v1,
        "re-selecting active must keep the rotated canonical, not downgrade to the snapshot"
    );
    assert_eq!(kc["Claude Code-credentials-a"].as_str().unwrap(), v1);
}

#[test]
fn switch_away_skips_sync_when_canonical_is_a_different_account() {
    // If the user logged into a different account directly (canonical no longer matches
    // the active profile), switching away must NOT overwrite the outgoing snapshot with
    // the foreign credentials.
    let (dir, claude_home, cs_home) = isolated();
    let a_v0 = fake_oauth_tagged("a@example.com", 3600, "a-v0");
    let b_blob = fake_oauth_tagged("b@example.com", 3600, "b-v0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &a_v0),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["a"])
        .assert()
        .success();

    // A *different* account is now in canonical (direct `claude /login`, no `cs save`).
    let foreign = fake_oauth_tagged("zzz@example.com", 3600, "z-v0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &foreign),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["b"])
        .assert()
        .success();

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        kc["Claude Code-credentials-a"].as_str().unwrap(),
        a_v0,
        "snapshot a must be untouched when canonical belongs to another account"
    );
}

#[test]
fn switch_away_preserves_rotated_token_for_emailless_blobs() {
    // Regression for the identity guard: two email-less blobs are the same account, so the
    // rotated token must still be synced on switch-away — otherwise email-less logins hit the
    // original switch-back logout the fix was meant to remove.
    let (dir, claude_home, cs_home) = isolated();
    let a_v0 = fake_oauth_no_email(3600, "a-v0");
    let b_blob = fake_oauth_no_email(3600, "b-v0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &a_v0),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["a"])
        .assert()
        .success();

    let a_v1 = fake_oauth_no_email(7200, "a-v1");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &a_v1),
            ("Claude Code-credentials-a", &a_v0),
            ("Claude Code-credentials-b", &b_blob),
        ],
    );
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["b"])
        .assert()
        .success();

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        kc["Claude Code-credentials-a"].as_str().unwrap(),
        a_v1,
        "email-less rotated token must still be synced on switch-away"
    );
}

#[test]
fn refresh_active_refreshes_canonical_in_place_and_mirrors_snapshot() {
    // The active profile is refreshed in place: the live canonical is rotated by Claude
    // Code and the result is mirrored into the snapshot — never rolled back to stale.
    let (dir, claude_home, cs_home) = isolated();
    let canon0 = fake_oauth_tagged("a@example.com", 3600, "canon0");
    let snap0 = fake_oauth_tagged("a@example.com", 3600, "snap0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &canon0),
            ("Claude Code-credentials-a", &snap0),
        ],
    );
    write_active(&cs_home, "a");

    let path = claude_shim_rotating(&dir, "canon0", "canon1");

    phase_c_env(&claude_home, &cs_home, &fixture)
        .env("PATH", &path)
        .args(["refresh", "a"])
        .assert()
        .success()
        .stderr(predicate::str::contains("refreshed `a`"));

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let canon1 = canon0.replace("canon0", "canon1");
    // Live session keeps the refreshed token (NOT rolled back to canon0).
    assert_eq!(kc["test-user"].as_str().unwrap(), canon1);
    // Snapshot mirrors the refreshed canonical.
    assert_eq!(kc["Claude Code-credentials-a"].as_str().unwrap(), canon1);
}

#[test]
fn refresh_active_unchanged_updates_stale_snapshot_without_login_prompt() {
    // Active profile whose live canonical is already fresh but whose snapshot lagged behind a
    // prior background rotation: refresh must NOT error with "run claude /login". It should
    // refresh the snapshot from the live creds and report the token still valid.
    let (dir, claude_home, cs_home) = isolated();
    let canonical_fresh = fake_oauth("a@example.com", 3600);
    let snap_expired = fake_oauth("a@example.com", -3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &canonical_fresh),
            ("Claude Code-credentials-a", &snap_expired),
        ],
    );
    write_active(&cs_home, "a");

    // Shim `claude` that does nothing: canonical is already current, so no rotation happens.
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim = bin_dir.join("claude");
    std::fs::write(&shim, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();
    }
    let mut path = bin_dir.into_os_string();
    path.push(":/usr/bin:/bin");

    phase_c_env(&claude_home, &cs_home, &fixture)
        .env("PATH", &path)
        .args(["refresh", "a"])
        .assert()
        .success()
        .stderr(predicate::str::contains("still valid"))
        .stderr(predicate::str::contains("claude /login").not());

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    assert_eq!(
        kc["Claude Code-credentials-a"].as_str().unwrap(),
        canonical_fresh,
        "stale snapshot should have been refreshed from the live canonical"
    );
}

#[test]
fn refresh_background_profile_persists_and_restores_canonical() {
    // A non-active profile is staged into canonical, refreshed, persisted, and then the
    // live canonical is restored to the active account — so the live session is never
    // left logged into the wrong (refreshed) account.
    let (dir, claude_home, cs_home) = isolated();
    let active_z = fake_oauth_tagged("z@example.com", 3600, "canonZ");
    let w0 = fake_oauth_tagged("w@example.com", 3600, "snap0");
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &active_z),
            ("Claude Code-credentials-z", &active_z),
            ("Claude Code-credentials-w", &w0),
        ],
    );
    write_active(&cs_home, "z");

    let path = claude_shim_rotating(&dir, "snap0", "snap1");

    phase_c_env(&claude_home, &cs_home, &fixture)
        .env("PATH", &path)
        .args(["refresh", "w"])
        .assert()
        .success()
        .stderr(predicate::str::contains("refreshed `w`"));

    let kc: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let w1 = w0.replace("snap0", "snap1");
    // Background profile got refreshed...
    assert_eq!(kc["Claude Code-credentials-w"].as_str().unwrap(), w1);
    // ...and the live canonical was restored to the active account (not left as w).
    assert_eq!(kc["test-user"].as_str().unwrap(), active_z);
}

/// Write an executable `claude` shim that simulates Claude Code refreshing the canonical
/// credential by rewriting `from`→`to` in the keychain fixture file, and return a `PATH`
/// value that resolves `claude` to it (keeping `/usr/bin:/bin` for `sed`).
fn claude_shim_rotating(dir: &TempDir, from: &str, to: &str) -> std::ffi::OsString {
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim = bin_dir.join("claude");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\nsed -i '' 's/{from}/{to}/g' \"$CS_TEST_KEYCHAIN_FIXTURE\"\nexit 0\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();
    }
    let mut path = bin_dir.into_os_string();
    path.push(":/usr/bin:/bin");
    path
}

#[test]
fn rm_deletes_profile_and_clears_active() {
    let (dir, claude_home, cs_home) = isolated();
    let work_blob = fake_oauth("work@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &work_blob),
            ("Claude Code-credentials-work", &work_blob),
        ],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["work"])
        .assert()
        .success();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["rm", "work"])
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert!(state["active"].is_null());
    assert!(!cs_home.join("profiles/work").exists());
}

#[test]
fn rename_preserves_active_pointer() {
    let (dir, claude_home, cs_home) = isolated();
    let work_blob = fake_oauth("work@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &work_blob),
            ("Claude Code-credentials-work", &work_blob),
        ],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["work"])
        .assert()
        .success();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["rename", "work", "office"])
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "office");
}

#[test]
fn default_then_default_go() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("a@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[("test-user", &blob), ("Claude Code-credentials-a", &blob)],
    );

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["default", "a"])
        .assert()
        .success();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["default-go"])
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "a");
    assert_eq!(state["default"], "a");
}

// --- master profile -----------------------------------------------------------

fn write_seed(claude_home: &std::path::Path) {
    std::fs::create_dir_all(claude_home.join("skills/foo")).unwrap();
    std::fs::write(claude_home.join("skills/foo/SKILL.md"), b"# foo skill\n").unwrap();
    std::fs::create_dir_all(claude_home.join("commands")).unwrap();
    std::fs::write(claude_home.join("commands/hello.md"), b"hello command\n").unwrap();
    std::fs::write(claude_home.join("CLAUDE.md"), b"top level\n").unwrap();
    // commands has content, agents/ does not exist (matches real machine).
}

fn dir_snapshot(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::collections::BTreeMap;
    let mut map = BTreeMap::new();
    fn walk(root: &std::path::Path, base: &std::path::Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(base).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(
                    format!("L:{rel}"),
                    target.to_string_lossy().into_owned().into_bytes(),
                );
            } else if meta.file_type().is_dir() {
                out.insert(format!("D:{rel}"), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(format!("F:{rel}"), std::fs::read(&path).unwrap());
            }
        }
    }
    walk(root, root, &mut map);
    map
}

fn master_env(
    claude_home: &std::path::Path,
    cs_home: &std::path::Path,
    fixture: &std::path::Path,
) -> Command {
    let mut c = cs();
    c.env("CLAUDE_HOME", claude_home)
        .env("CS_HOME", cs_home)
        .env("CS_TEST_KEYCHAIN_FIXTURE", fixture);
    c
}

fn seeded_master_setup() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let (dir, claude_home, cs_home) = isolated();
    write_seed(&claude_home);
    let blob = fake_oauth("personal@example.com", 3600);
    // Only the canonical entry — `cs save personal` will create the profile entry.
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);
    (dir, claude_home, cs_home, fixture)
}

#[test]
fn master_set_then_uninstall_is_byte_clean() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;
    let before = dir_snapshot(&claude_home);

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();

    // Validate symlinks now exist and point into the personal profile dir.
    let target = std::fs::read_link(claude_home.join("skills")).unwrap();
    assert!(
        target.starts_with(cs_home.join("profiles/personal")),
        "skills symlink should point into profiles/personal: {}",
        target.display()
    );
    assert!(std::fs::symlink_metadata(claude_home.join("CLAUDE.md"))
        .unwrap()
        .file_type()
        .is_symlink());

    master_env(&claude_home, &cs_home, &fixture)
        .args(["uninstall"])
        .assert()
        .success();

    let after = dir_snapshot(&claude_home);
    assert_eq!(before, after, "master set→uninstall is not byte-clean");
}

#[test]
fn master_set_idempotent() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();
    // Second invocation: same master, no-op.
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already symlinked"));
}

#[test]
fn master_status_reports_designated_master() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();

    let output = master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["master"], "personal");
    assert_eq!(v["items"].as_array().unwrap().len(), 4);
}

#[test]
fn master_change_moves_content() {
    let (dir, claude_home, cs_home) = isolated();
    write_seed(&claude_home);
    let blob = fake_oauth("a@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "work"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();

    // Switch master to work — work has none of the four candidates.
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "work"])
        .assert()
        .success();

    let target = std::fs::read_link(claude_home.join("skills")).unwrap();
    assert!(
        target.starts_with(cs_home.join("profiles/work")),
        "skills should now point into work: {}",
        target.display()
    );
    assert!(cs_home.join("profiles/work/skills/foo/SKILL.md").exists());
    assert!(!cs_home.join("profiles/personal/skills").exists());

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["master"], "work");
}

#[test]
fn master_change_refuses_when_target_non_empty() {
    let (dir, claude_home, cs_home) = isolated();
    write_seed(&claude_home);
    let blob = fake_oauth("a@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "work"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();

    // Manually plant content in the work profile dir to block the change.
    std::fs::create_dir_all(cs_home.join("profiles/work/skills/blocker")).unwrap();
    std::fs::write(
        cs_home.join("profiles/work/skills/blocker/SKILL.md"),
        b"blocker\n",
    )
    .unwrap();

    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "work"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn rm_master_profile_refuses() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();

    master_env(&claude_home, &cs_home, &fixture)
        .args(["rm", "personal"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("master profile"))
        .stderr(predicate::str::contains("cs master --unset"));
}

#[test]
fn rename_master_profile_updates_state_and_symlinks() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["rename", "personal", "personal2"])
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["master"], "personal2");

    let target = std::fs::read_link(claude_home.join("skills")).unwrap();
    assert!(
        target.starts_with(cs_home.join("profiles/personal2")),
        "skills should now point into profiles/personal2: {}",
        target.display()
    );
    assert!(cs_home
        .join("profiles/personal2/skills/foo/SKILL.md")
        .exists());
}

#[test]
fn master_unset_restores_claude_home() {
    let (dir, claude_home, cs_home, fixture) = seeded_master_setup();
    let _ = dir;
    let before = dir_snapshot(&claude_home);

    master_env(&claude_home, &cs_home, &fixture)
        .args(["save", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "personal"])
        .assert()
        .success();
    master_env(&claude_home, &cs_home, &fixture)
        .args(["master", "--unset"])
        .assert()
        .success();

    // ~/.claude should be back to the seeded state (no symlinks).
    assert!(!std::fs::symlink_metadata(claude_home.join("skills"))
        .unwrap()
        .file_type()
        .is_symlink());
    let after = dir_snapshot(&claude_home);
    assert_eq!(before, after, "master --unset is not byte-clean");

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert!(state["master"].is_null());
}

#[test]
fn status_no_active_json_shape() {
    let (_dir, claude_home, cs_home) = isolated();
    let output = cs()
        .env("CLAUDE_HOME", &claude_home)
        .env("CS_HOME", &cs_home)
        .args(["status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
    for k in ["active", "default", "previous", "asked_about"] {
        assert!(v.get(k).is_some(), "missing {k}");
    }
}

// --- usage % view -------------------------------------------------------------

#[test]
fn save_rejects_path_traversal_name() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["save", "foo/bar"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn rm_rejects_path_traversal_name() {
    // `cs rm ../../foo` must be refused before any filesystem touch — otherwise profile_dir
    // resolves outside the profiles tree and remove_dir_all could delete an arbitrary dir.
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["rm", "../../foo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn rename_rejects_path_traversal_from() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["rename", "../evil", "ok"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn master_rejects_path_traversal_name() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["master", ".."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn save_rejects_dotfile_name() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["save", ".dotfile"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid profile name"));
}

#[test]
fn rm_refuses_to_delete_through_symlinked_profile_dir() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob),
            ("Claude Code-credentials-sneaky", &blob),
        ],
    );

    // Plant a real directory somewhere outside cs_home, then symlink the
    // profile dir to it. `cs rm sneaky` must refuse rather than chase
    // the symlink and `rm -rf` the real target.
    let outside = dir.path().join("outside-target");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("important.txt"), b"do not delete").unwrap();

    let profiles = cs_home.join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    let link = profiles.join("sneaky");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["rm", "sneaky"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("symlink"));

    assert!(outside.join("important.txt").exists(),
        "rm chased a symlink and deleted the real target");
    assert!(link.exists(), "symlink itself should be intact");
}

#[test]
fn refresh_kills_claude_after_timeout() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("work@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob),
            ("Claude Code-credentials-work", &blob),
        ],
    );

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim = bin_dir.join("claude");
    std::fs::write(&shim, "#!/bin/sh\nsleep 9999\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();
    }

    // Prepend the shim dir so `claude` resolves to it; keep /bin and
    // /usr/bin so the shim itself can find `sleep`.
    let mut path = bin_dir.as_os_str().to_owned();
    path.push(":/usr/bin:/bin");

    let started = std::time::Instant::now();
    phase_c_env(&claude_home, &cs_home, &fixture)
        .env("PATH", &path)
        .args(["refresh", "work"])
        .timeout(std::time::Duration::from_secs(70))
        .assert()
        .failure()
        .stderr(predicate::str::contains("timed out"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(70),
        "refresh did not bound the subprocess (took {:?})",
        started.elapsed()
    );
}

#[test]
fn setup_refuses_when_rc_is_unreadable_and_leaves_it_intact() {
    // A `.zshrc` with invalid UTF-8 must abort `cs setup` with an error,
    // not silently overwrite the user's file with a blank wrapper.
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let rc = home.join(".zshrc");
    let original: &[u8] = &[0xFF, 0xFE, b'\n'];
    std::fs::write(&rc, original).unwrap();

    cs().env("HOME", &home)
        .args(["setup", "--shell", "zsh"])
        .assert()
        .failure();

    assert_eq!(std::fs::read(&rc).unwrap(), original);
}

#[test]
fn usage_default_shows_pct_columns() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("work@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob),
            ("Claude Code-credentials-work", &blob),
        ],
    );

    let limits_dir = dir.path().join("limits");
    std::fs::create_dir_all(&limits_dir).unwrap();
    std::fs::write(
        limits_dir.join("work.json"),
        br#"{
            "five_hour":  { "utilization": 37, "resets_at": "2099-01-01T00:00:00Z" },
            "seven_day":  { "utilization": 64, "resets_at": "2099-01-01T00:00:00Z" },
            "seven_day_sonnet": null,
            "seven_day_opus":   null,
            "extra_usage":      { "is_enabled": false }
        }"#,
    )
    .unwrap();

    let json = phase_c_env(&claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .args(["usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    let row = &v["rows"][0];
    assert_eq!(row["profile"], "work");
    assert_eq!(row["five_h_pct_left"], 63);
    assert_eq!(row["weekly_pct_left"], 36);
    assert!(row["error"].is_null());

    let text_out = phase_c_env(&claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .arg("usage")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(text_out).unwrap();
    assert!(text.contains("5H LEFT"), "missing 5H LEFT header: {text}");
    assert!(text.contains("63%"), "missing 63% cell: {text}");
}

#[test]
fn usage_token_expired_shows_dash() {
    let (dir, claude_home, cs_home) = isolated();
    let canonical = fake_oauth("primary@example.com", 3600);
    // The work profile's OAuth blob is already expired.
    let work_expired = fake_oauth("work@example.com", -3_600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &canonical),
            ("Claude Code-credentials-work", &work_expired),
        ],
    );

    let json = phase_c_env(&claude_home, &cs_home, &fixture)
        .args(["usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    let row = &v["rows"][0];
    assert_eq!(row["profile"], "work");
    assert!(row["five_h_pct_left"].is_null());
    let err = row["error"].as_str().expect("error string set");
    assert!(err.contains("token expired"), "unexpected error: {err}");
    assert!(err.contains("cs refresh"), "missing refresh hint: {err}");
}

#[test]
fn usage_rate_limited_serves_cached_then_warns() {
    let (dir, claude_home, cs_home) = isolated();
    let blob = fake_oauth("work@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob),
            ("Claude Code-credentials-work", &blob),
        ],
    );

    // Prime the on-disk cache so rate_limited can fall back to it.
    let cache_dir = cs_home.join("cache").join("usage-limits");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(
        cache_dir.join("work.json"),
        br#"{
            "fetched_at_unix": 1700000000,
            "payload": {
                "five_hour": { "utilization": 20, "resets_at": null },
                "seven_day": { "utilization": 50, "resets_at": null },
                "seven_day_sonnet": null,
                "seven_day_opus": null
            }
        }"#,
    )
    .unwrap();

    let fail_dir = dir.path().join("fail");
    std::fs::create_dir_all(&fail_dir).unwrap();
    std::fs::write(fail_dir.join("work.txt"), b"rate_limited").unwrap();

    let json = phase_c_env(&claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FAIL", &fail_dir)
        .args(["usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
    let row = &v["rows"][0];
    assert_eq!(row["five_h_pct_left"], 80);
    assert_eq!(row["weekly_pct_left"], 50);
    assert!(row["error"].is_null());
    let warnings = v["warnings"].as_array().expect("warnings array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("rate-limited")),
        "expected rate-limited warning: {warnings:?}"
    );
}

// --- auto-switch ---------------------------------------------------------------

fn auto_switch_env(
    home: &std::path::Path,
    claude_home: &std::path::Path,
    cs_home: &std::path::Path,
    fixture: &std::path::Path,
) -> Command {
    let mut c = cs();
    c.env("HOME", home)
        .env("CLAUDE_HOME", claude_home)
        .env("CS_HOME", cs_home)
        .env("CS_TEST_KEYCHAIN_FIXTURE", fixture)
        .env("CS_TEST_NO_LAUNCHCTL", "1")
        .env("CS_TEST_NO_NOTIFY", "1");
    c
}

fn auto_switch_setup() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let claude_home = home.join(".claude");
    let cs_home = home.join(".claude-cs");
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::create_dir_all(&cs_home).unwrap();
    std::fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    (dir, home, claude_home, cs_home)
}

#[test]
fn auto_switch_status_off_when_no_settings() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .arg("auto-switch")
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-switch: off"));
}

#[test]
fn auto_switch_on_writes_plist_and_flips_flag() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .args(["auto-switch", "on"])
        .assert()
        .success()
        .stderr(predicate::str::contains("auto-switch: on"));

    let plist = home.join("Library/LaunchAgents/com.claude-switch.autoswitch.plist");
    assert!(plist.exists(), "plist missing at {}", plist.display());
    let plist_text = std::fs::read_to_string(&plist).unwrap();
    assert!(plist_text.contains("__autoswitch-tick"));
    assert!(plist_text.contains("<integer>300</integer>"));

    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["auto_switch"], true);
}

#[test]
fn auto_switch_off_removes_plist_and_flips_flag() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .args(["auto-switch", "on"])
        .assert()
        .success();
    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .args(["auto-switch", "off"])
        .assert()
        .success()
        .stderr(predicate::str::contains("auto-switch: off"));

    let plist = home.join("Library/LaunchAgents/com.claude-switch.autoswitch.plist");
    assert!(!plist.exists(), "plist should have been removed");

    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["auto_switch"], false);
}

#[test]
fn uninstall_clears_auto_switch_artifacts() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .args(["auto-switch", "on"])
        .assert()
        .success();
    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .arg("uninstall")
        .assert()
        .success();

    let plist = home.join("Library/LaunchAgents/com.claude-switch.autoswitch.plist");
    assert!(!plist.exists(), "plist should have been removed by uninstall");
    assert!(
        !cs_home.join("settings.json").exists(),
        "settings.json should have been removed by uninstall"
    );
}

#[test]
fn uninstall_is_idempotent_when_auto_switch_never_enabled() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob = fake_oauth("primary@example.com", 3600);
    let fixture = fixture_path(dir.path(), &[("test-user", &blob)]);

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .arg("uninstall")
        .assert()
        .success();
}

fn write_fixture_limits(dir: &std::path::Path, profile: &str, five: f64, seven: f64) {
    std::fs::create_dir_all(dir).unwrap();
    let payload = format!(
        r#"{{
            "five_hour":  {{ "utilization": {five}, "resets_at": "2099-01-01T00:00:00Z" }},
            "seven_day":  {{ "utilization": {seven}, "resets_at": "2099-01-01T00:00:00Z" }},
            "seven_day_sonnet": null,
            "seven_day_opus": null,
            "extra_usage": {{ "is_enabled": false }}
        }}"#
    );
    std::fs::write(dir.join(format!("{profile}.json")), payload).unwrap();
}

fn write_settings(cs_home: &std::path::Path, auto_switch: bool) {
    std::fs::create_dir_all(cs_home).unwrap();
    let body = serde_json::json!({ "auto_switch": auto_switch });
    std::fs::write(cs_home.join("settings.json"), body.to_string()).unwrap();
}

fn write_active(cs_home: &std::path::Path, name: &str) {
    std::fs::create_dir_all(cs_home).unwrap();
    let body = serde_json::json!({ "active": name });
    std::fs::write(cs_home.join("state.json"), body.to_string()).unwrap();
}

#[test]
fn autoswitch_tick_swaps_when_active_capped() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob_a = fake_oauth("a@example.com", 3600);
    let blob_b = fake_oauth("b@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob_a),
            ("Claude Code-credentials-a", &blob_a),
            ("Claude Code-credentials-b", &blob_b),
        ],
    );
    let limits_dir = dir.path().join("limits");
    write_fixture_limits(&limits_dir, "a", 100.0, 50.0);
    write_fixture_limits(&limits_dir, "b", 20.0, 10.0);
    write_settings(&cs_home, true);
    write_active(&cs_home, "a");

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .arg("__autoswitch-tick")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "b", "tick should have switched to b");

    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("settings.json")).unwrap()).unwrap();
    assert!(settings["last_switch_unix"].is_number());
}

#[test]
fn autoswitch_tick_skips_stale_candidate() {
    // A 429-stale candidate must not be chosen as a switch target (symmetric to the active
    // stale gate). Active `a` is capped and fresh; the only other profile `b` is served from
    // rate-limited stale cache showing healthy — the tick must NOT switch to it.
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob_a = fake_oauth("a@example.com", 3600);
    let blob_b = fake_oauth("b@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob_a),
            ("Claude Code-credentials-a", &blob_a),
            ("Claude Code-credentials-b", &blob_b),
        ],
    );
    let limits_dir = dir.path().join("limits");
    write_fixture_limits(&limits_dir, "a", 100.0, 50.0); // active: capped, fresh

    // `b` is rate-limited and only available from primed (stale) cache showing healthy.
    let fail_dir = dir.path().join("fail");
    std::fs::create_dir_all(&fail_dir).unwrap();
    std::fs::write(fail_dir.join("b.txt"), b"rate_limited").unwrap();
    let cache_dir = cs_home.join("cache").join("usage-limits");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(
        cache_dir.join("b.json"),
        br#"{ "fetched_at_unix": 1700000000,
              "payload": { "five_hour": {"utilization": 20, "resets_at": null},
                           "seven_day": {"utilization": 10, "resets_at": null},
                           "seven_day_sonnet": null, "seven_day_opus": null } }"#,
    )
    .unwrap();

    write_settings(&cs_home, true);
    write_active(&cs_home, "a");

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .env("CS_TEST_LIMITS_FAIL", &fail_dir)
        .arg("__autoswitch-tick")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "a", "must not switch to a stale candidate");
}

#[test]
fn autoswitch_tick_no_op_when_disabled() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob_a = fake_oauth("a@example.com", 3600);
    let blob_b = fake_oauth("b@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob_a),
            ("Claude Code-credentials-a", &blob_a),
            ("Claude Code-credentials-b", &blob_b),
        ],
    );
    let limits_dir = dir.path().join("limits");
    write_fixture_limits(&limits_dir, "a", 100.0, 50.0);
    write_fixture_limits(&limits_dir, "b", 20.0, 10.0);
    write_settings(&cs_home, false);
    write_active(&cs_home, "a");

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .arg("__autoswitch-tick")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "a", "tick should not have switched");
}

#[test]
fn autoswitch_tick_no_op_when_active_healthy() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob_a = fake_oauth("a@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob_a),
            ("Claude Code-credentials-a", &blob_a),
        ],
    );
    let limits_dir = dir.path().join("limits");
    write_fixture_limits(&limits_dir, "a", 50.0, 50.0);
    write_settings(&cs_home, true);
    write_active(&cs_home, "a");

    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .arg("__autoswitch-tick")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "a", "healthy active must not be touched");
}

#[test]
fn autoswitch_tick_aborts_when_state_changes_under_us() {
    let (dir, home, claude_home, cs_home) = auto_switch_setup();
    let blob_a = fake_oauth("a@example.com", 3600);
    let blob_b = fake_oauth("b@example.com", 3600);
    let blob_c = fake_oauth("c@example.com", 3600);
    let fixture = fixture_path(
        dir.path(),
        &[
            ("test-user", &blob_a),
            ("Claude Code-credentials-a", &blob_a),
            ("Claude Code-credentials-b", &blob_b),
            ("Claude Code-credentials-c", &blob_c),
        ],
    );
    let limits_dir = dir.path().join("limits");
    write_fixture_limits(&limits_dir, "a", 100.0, 50.0);
    write_fixture_limits(&limits_dir, "b", 20.0, 10.0);
    write_fixture_limits(&limits_dir, "c", 50.0, 30.0);
    write_settings(&cs_home, true);
    write_active(&cs_home, "a");

    // The tick will see active=A, decide to switch to B, then inside the lock
    // the test hook flips state.active to C — the tick must abort.
    auto_switch_env(&home, &claude_home, &cs_home, &fixture)
        .env("CS_TEST_LIMITS_FIXTURE", &limits_dir)
        .env("CS_TEST_AUTOSWITCH_PRE_LOCK_STATE_ACTIVE", "c")
        .arg("__autoswitch-tick")
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cs_home.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["active"], "c", "tick must defer to the racing writer");
}
