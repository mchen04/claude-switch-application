use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::{GlobalOpts, OptionalNameArg};
use crate::error::{Error, Result};
use crate::keychain::{self, Keychain};
use crate::lock::CsLock;
use crate::paths::Paths;
use crate::profile::OauthCreds;
use crate::state::State;

/// How long we wait for `claude /status` to refresh a credential before giving up.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// Strategy: Anthropic doesn't expose a public OAuth refresh endpoint for first-party
/// Claude Code creds, so we delegate refresh to the `claude` CLI itself, which refreshes
/// whatever credential currently lives in the canonical Keychain entry.
///
/// There are two cases, and they are handled very differently to avoid ever logging the
/// user out:
///
/// * **Active profile** — its creds already *are* the canonical entry, so we let
///   `claude /status` refresh it in place and then mirror the rotated result back into
///   the profile snapshot. Nothing is staged and nothing is rolled back, so the live
///   session can never be left holding a dead token.
/// * **Background profile** — we must stage its stale blob into canonical for the
///   duration of the refresh, then restore the real canonical afterwards. The active
///   account's refresh token is never used during this window (we only rotate the
///   staged profile's token), so restoring it is always safe.
pub fn run(
    paths: &Paths,
    kc: &dyn Keychain,
    _global: &GlobalOpts,
    args: &OptionalNameArg,
) -> Result<()> {
    let state = State::load(&paths.state_file()).unwrap_or_default();
    let name = args
        .name
        .clone()
        .or_else(|| state.active.clone())
        .ok_or(Error::NoActiveProfile)?;
    let target = keychain::profile_account(&name);

    let stale = kc
        .read(&target)
        .map_err(|_| Error::ProfileNotFound(name.clone()))?;
    let creds = OauthCreds::parse(&stale)?;

    if which("claude").is_none() {
        return Err(Error::Other(
            "`claude` CLI not on PATH; run `claude /login` for this profile manually".into(),
        ));
    }

    let _lock = CsLock::acquire(paths)?;

    let canonical = keychain::canonical_account();
    // Only take the in-place fast path when the canonical entry genuinely holds *this*
    // profile's account. If state says this profile is active but the user logged into a
    // different account directly, the canonical creds are someone else's — fall back to
    // the staged path, which refreshes the named profile without assuming canonical is it.
    let canonical_is_this_profile = state.active.as_deref() == Some(name.as_str())
        && kc
            .read(&canonical)
            .ok()
            .and_then(|b| OauthCreds::parse(&b).ok())
            .map(|c| !crate::profile::is_cross_account(c.email(), creds.email()))
            .unwrap_or(false);

    if canonical_is_this_profile {
        refresh_active(kc, &canonical, &target, &name, &creds)
    } else {
        refresh_staged(kc, &canonical, &target, &name, &stale, &creds)
    }
}

/// Refresh the active profile by letting Claude Code rotate the canonical entry in
/// place, then snapshotting the result back into the profile entry.
fn refresh_active(
    kc: &dyn Keychain,
    canonical: &str,
    target: &str,
    name: &str,
    creds: &OauthCreds,
) -> Result<()> {
    let before = kc.read(canonical).map_err(|_| {
        Error::Other("no active Claude credential in the Keychain; run `claude /login`".into())
    })?;

    // On failure the canonical entry is untouched (we never staged anything), so the live
    // session keeps its existing creds — no rollback needed.
    run_claude_status()?;

    let after = kc.read(canonical)?;

    // `after` is the authoritative live credential. Mirror it into the profile snapshot when it
    // is the same account (idempotent), so the snapshot tracks the live token even if Claude
    // Code had nothing to rotate. Crucially, judge validity by the LIVE credential, not the
    // possibly-staler snapshot — otherwise a fresh-canonical / stale-snapshot active profile
    // would wrongly report "did not refresh; run claude /login" when nothing is actually wrong.
    let live = match OauthCreds::parse(&after) {
        Ok(fresh) if !crate::profile::is_cross_account(fresh.email(), creds.email()) => {
            keychain::write_verified(kc, target, &after)?;
            Some(fresh)
        }
        _ => {
            tracing::warn!(
                profile = %name,
                "refreshed canonical identity differs from profile snapshot; left snapshot untouched"
            );
            None
        }
    };

    if after == before {
        // Claude Code left canonical unchanged — it was already current; report on the live creds.
        return unchanged_result(live.as_ref().unwrap_or(creds), name);
    }
    eprintln!("refreshed `{name}`");
    Ok(())
}

/// Refresh a background (non-active) profile by temporarily staging its blob into the
/// canonical entry, refreshing, persisting the result, and restoring canonical.
fn refresh_staged(
    kc: &dyn Keychain,
    canonical: &str,
    target: &str,
    name: &str,
    stale: &[u8],
    creds: &OauthCreds,
) -> Result<()> {
    let prev_canonical = kc.read(canonical).ok();

    if running_claude_processes() > 0 {
        eprintln!(
            "warning: `claude` is running; refreshing a background profile briefly swaps the \
             live credential. Quit claude, or run `cs refresh` for the active profile instead."
        );
    }

    // Stage the stale creds into canonical so `claude /status` refreshes *them*.
    kc.write(canonical, stale)?;
    match kc.read(canonical) {
        Ok(b) if b == stale => {}
        _ => {
            rollback_canonical(kc, canonical, prev_canonical.as_deref());
            return Err(Error::Other(
                "canonical Keychain staging write failed; rolled back to previous".into(),
            ));
        }
    }

    if let Err(e) = run_claude_status() {
        rollback_canonical(kc, canonical, prev_canonical.as_deref());
        return Err(e);
    }

    let refreshed = match kc.read(canonical) {
        Ok(b) => b,
        Err(e) => {
            rollback_canonical(kc, canonical, prev_canonical.as_deref());
            return Err(Error::Keychain(format!("read refreshed canonical: {e}")));
        }
    };

    if refreshed == stale {
        restore_after_staged(kc, canonical, prev_canonical.as_deref(), stale);
        return unchanged_result(creds, name);
    }

    // A concurrent live `claude` session can rotate *its own* account's token over our
    // staged blob during the window. If canonical now holds a different account, that
    // fresher write is the live session's — never copy it into this profile, and never
    // clobber it with our stale restore, or we'd downgrade the live account to a dead token.
    let refreshed_is_this_profile = OauthCreds::parse(&refreshed)
        .map(|r| !crate::profile::is_cross_account(r.email(), creds.email()))
        .unwrap_or(false);
    if !refreshed_is_this_profile {
        tracing::warn!(
            profile = %name,
            "canonical changed accounts during refresh; leaving the live credential in place"
        );
        return Err(Error::Other(format!(
            "another `claude` session changed the live credential while refreshing `{name}`; \
             quit claude and retry, or run `cs refresh` for the active profile"
        )));
    }

    // Persist into the profile entry first; only then restore canonical, so a failure to
    // save the refreshed creds still leaves the active account restored.
    if let Err(e) = keychain::write_verified(kc, target, &refreshed) {
        restore_after_staged(kc, canonical, prev_canonical.as_deref(), &refreshed);
        return Err(e);
    }
    restore_after_staged(kc, canonical, prev_canonical.as_deref(), &refreshed);
    eprintln!("refreshed `{name}`");
    Ok(())
}

/// Put the live account's credential back after a staged refresh. Restores only when
/// canonical still holds `expected` (the blob we left there); if a concurrent live session
/// wrote a fresher blob during the window, leave it untouched rather than downgrade the
/// live credential. When there was no prior canonical (no active login at all), clear the
/// slot so refreshing a background profile never installs it as the live account.
fn restore_after_staged(kc: &dyn Keychain, canonical: &str, prev: Option<&[u8]>, expected: &[u8]) {
    if let Ok(now) = kc.read(canonical) {
        if now != expected {
            tracing::warn!(
                "canonical was updated by another process during staged refresh; leaving it in place"
            );
            return;
        }
    }
    match prev {
        Some(p) => rollback_canonical(kc, canonical, Some(p)),
        None => {
            if let Err(e) = kc.delete(canonical) {
                tracing::warn!(error = %e, "failed to clear canonical after background refresh");
            }
        }
    }
}

/// Shared handling for "Claude Code returned the same blob it was given". If the token is
/// genuinely expired that's a hard failure; otherwise it was simply still valid.
fn unchanged_result(creds: &OauthCreds, name: &str) -> Result<()> {
    if creds.is_expired(Duration::from_secs(0)) {
        return Err(Error::Other(format!(
            "Claude Code did not refresh `{name}`; run `claude /login` for this profile manually"
        )));
    }
    match creds.expires_in() {
        Some(d) => eprintln!(
            "`{name}` token still valid for {}; nothing to refresh",
            crate::profile::human_duration(d.as_secs())
        ),
        None => eprintln!("`{name}` token still valid; nothing to refresh"),
    }
    Ok(())
}

/// Spawn `claude /status` and wait for it to exit, bounding the wait at
/// [`REFRESH_TIMEOUT`] and killing the child if it overruns. Returns the subprocess
/// error (timeout, non-zero exit, spawn failure) without touching the Keychain — callers
/// own any rollback.
fn run_claude_status() -> Result<()> {
    let subproc_err = |message: String| Error::Subprocess {
        cmd: "claude /status".into(),
        message,
    };

    let mut child = Command::new("claude")
        .args(["/status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| subproc_err(e.to_string()))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut stderr = Vec::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_end(&mut stderr);
                }
                return Err(subproc_err(format!(
                    "exit {}: {}",
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&stderr)
                )));
            }
            Ok(None) => {
                if started.elapsed() > REFRESH_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(subproc_err(format!(
                        "timed out after {}s",
                        REFRESH_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(subproc_err(e.to_string()));
            }
        }
    }
}

fn rollback_canonical(kc: &dyn Keychain, canonical: &str, prev: Option<&[u8]>) {
    let Some(prev) = prev else { return };
    if let Err(e) = kc.write(canonical, prev) {
        eprintln!("error: could not restore canonical keychain entry {canonical}: {e}");
        tracing::error!(account = %canonical, error = %e, "canonical keychain restore failed");
    }
}

fn running_claude_processes() -> usize {
    let out = Command::new("/usr/bin/pgrep").args(["-x", "claude"]).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        _ => 0,
    }
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&path) {
        let candidate = p.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
