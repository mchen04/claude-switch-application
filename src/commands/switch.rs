use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::cli::GlobalOpts;
use crate::error::{Error, Result};
use crate::keychain::{self, Keychain};
use crate::lock::CsLock;
use crate::paths::Paths;
use crate::profile::OauthCreds;
use crate::state::State;

pub fn run(
    paths: &Paths,
    kc: &dyn Keychain,
    global: &GlobalOpts,
    target_name: &str,
    passthrough: &[String],
) -> Result<()> {
    crate::paths::validate_profile_name(target_name)?;
    let _lock = CsLock::acquire(paths)?;
    paths.ensure_cs_home()?;
    run_locked(paths, kc, global, target_name)?;

    if !passthrough.is_empty() {
        let err = Command::new("claude").args(passthrough).exec();
        return Err(Error::Subprocess {
            cmd: "claude".into(),
            message: err.to_string(),
        });
    }
    Ok(())
}

/// Same as [`run`] minus the lock acquisition and the `claude` exec. Callers
/// (currently the auto-switch tick) MUST already hold a [`CsLock`] before
/// invoking this so the re-check it performed before deciding to switch
/// remains valid through the swap.
pub(crate) fn run_locked(
    paths: &Paths,
    kc: &dyn Keychain,
    global: &GlobalOpts,
    target_name: &str,
) -> Result<()> {
    let canonical = keychain::canonical_account();
    let prev_canonical_blob = kc.read(&canonical).ok();
    let state_path = paths.state_file();
    let mut state = State::load(&state_path).unwrap_or_default();
    let prior_active = state.active.clone();

    // Preserve the outgoing account's *live* credentials before we overwrite the
    // canonical entry. Claude Code rotates the OAuth access+refresh token in the
    // canonical entry as it runs, and a rotated refresh token invalidates its
    // predecessor server-side. The profile snapshot saved by `cs` never sees
    // those rotations, so without this flush, switching away and later back
    // installs a long-dead refresh token and Claude Code logs the user out. Copy
    // the freshest canonical blob into the outgoing profile entry first. This
    // runs before we read the target, so re-selecting the active profile reads
    // back the just-synced creds instead of downgrading canonical to a snapshot.
    if let (Some(prev_name), Some(prev_blob)) =
        (prior_active.as_deref(), prev_canonical_blob.as_deref())
    {
        sync_canonical_to_profile(kc, prev_name, prev_blob);
    }

    let claude_target = read_target_claude(kc, target_name)?;
    let prev_settings = fs::read(paths.claude_settings()).ok();

    let target_creds = OauthCreds::parse(&claude_target)?;
    if target_creds.is_expired(std::time::Duration::from_secs(60)) {
        eprintln!(
            "warning: target Claude profile `{}` token is near expiry; consider `cs refresh {}` first",
            target_name, target_name
        );
    }

    kc.write(&canonical, &claude_target)?;
    match kc.read(&canonical) {
        Ok(b) if b == claude_target => {}
        Ok(_) | Err(_) => {
            rollback_claude(kc, &canonical, prev_canonical_blob.as_deref());
            return Err(Error::Other(
                "canonical Keychain write verification failed; rolled back to previous".into(),
            ));
        }
    }

    let profile_settings = paths.profile_claude_settings(target_name);
    if profile_settings.exists() {
        if let Err(e) = atomic_replace(&profile_settings, &paths.claude_settings()) {
            rollback_claude(kc, &canonical, prev_canonical_blob.as_deref());
            if let Some(prev) = prev_settings.as_deref() {
                let _ = write_bytes_atomic(&paths.claude_settings(), prev);
            }
            return Err(e);
        }
    }

    if prior_active.as_deref() != Some(target_name) {
        state.previous = prior_active;
    }
    state.active = Some(target_name.to_string());
    state.save(&state_path)?;

    let marker = paths.active_profile_marker();
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&marker, target_name.as_bytes()) {
        tracing::warn!(?marker, error=%e, "could not write .active-profile marker");
    }

    if !global.json {
        eprintln!("switched -> {target_name} (claude)");
    }

    if running_claude_processes() > 0 {
        eprintln!(
            "note: detected running `claude` process(es); restart them to pick up the new account"
        );
    }
    Ok(())
}

pub fn run_previous(
    paths: &Paths,
    kc: &dyn Keychain,
    global: &GlobalOpts,
    passthrough: &[String],
) -> Result<()> {
    let state = State::load(&paths.state_file()).unwrap_or_default();
    let prev = state.previous.clone().ok_or(Error::NoPreviousProfile)?;
    run(paths, kc, global, &prev, passthrough)
}

fn read_target_claude(kc: &dyn Keychain, target_name: &str) -> Result<Vec<u8>> {
    let target_account = keychain::profile_account(target_name);
    let target_blob = kc
        .read(&target_account)
        .map_err(|_| Error::ProfileNotFound(target_name.to_string()))?;
    OauthCreds::parse(&target_blob)?;
    Ok(target_blob)
}

fn rollback_claude(kc: &dyn Keychain, canonical: &str, prev: Option<&[u8]>) {
    if let Some(prev) = prev {
        if let Err(e) = kc.write(canonical, prev) {
            eprintln!("error: keychain rollback failed for {canonical}: {e}");
            tracing::error!(account = %canonical, error = %e, "keychain rollback failed");
        }
    }
}

/// Copy the canonical (live) credential blob back into the outgoing profile's
/// Keychain entry so its snapshot keeps pace with Claude Code's background token
/// rotation. Best-effort and identity-guarded: only writes when `blob` is valid
/// OAuth creds for the *same account* the profile already holds (matched by
/// email), so a canonical that belongs to a different account the user logged
/// into directly never overwrites the wrong snapshot. Skips when the profile
/// entry is absent (we never resurrect a removed profile) and never deletes the
/// existing snapshot on failure — at worst the snapshot stays as stale as it was
/// before, which is the pre-fix behaviour, so this can only help.
fn sync_canonical_to_profile(kc: &dyn Keychain, profile_name: &str, blob: &[u8]) {
    let account = keychain::profile_account(profile_name);
    let Ok(existing_blob) = kc.read(&account) else {
        return; // no snapshot for the outgoing profile — nothing to keep in sync
    };
    if existing_blob == blob {
        return; // already current
    }
    // Only refuse the sync when we can prove the canonical blob is a *different* account
    // (e.g. the user ran `claude /login` as someone else without `cs save`). An
    // unparseable blob on either side is treated as "can't prove same account" and also
    // skipped. Crucially, two email-less blobs are NOT a mismatch — see is_cross_account.
    let cross_account = match (OauthCreds::parse(blob), OauthCreds::parse(&existing_blob)) {
        (Ok(incoming), Ok(existing)) => {
            crate::profile::is_cross_account(incoming.email(), existing.email())
        }
        _ => true,
    };
    if cross_account {
        tracing::warn!(
            profile = %profile_name,
            "canonical credential is a different account than the saved profile; not syncing"
        );
        return;
    }
    if let Err(e) = kc.write(&account, blob) {
        tracing::warn!(profile = %profile_name, error = %e, "failed to sync rotated token into profile");
        return;
    }
    match kc.read(&account) {
        Ok(b) if b == blob => {
            tracing::info!(profile = %profile_name, "synced rotated canonical token into outgoing profile");
        }
        _ => tracing::warn!(
            profile = %profile_name,
            "sync read-back mismatch; profile snapshot may still be stale"
        ),
    }
}

fn atomic_replace(src: &Path, dst: &Path) -> Result<()> {
    let bytes = fs::read(src).map_err(|e| Error::io_at(src, e))?;
    write_bytes_atomic(dst, &bytes)
}

fn write_bytes_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    crate::jsonio::atomic_write_bytes(dst, bytes)
}

fn running_claude_processes() -> usize {
    let out = Command::new("/usr/bin/pgrep")
        .args(["-x", "claude"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        _ => 0,
    }
}
