use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::auto_switch::{self, Decision};
use crate::cli::GlobalOpts;
use crate::error::Result;
use crate::keychain::{self, Keychain};
use crate::lock::CsLock;
use crate::paths::Paths;
use crate::profile::OauthCreds;
use crate::settings::Settings;
use crate::state::State;
use crate::usage::limits::{self, UsageLimits, CACHE_MAX_AGE};

const ITERATION_BUDGET: Duration = Duration::from_secs(10);
const CAPPED_NOTIFY_THROTTLE: u64 = 3600;

pub fn run(paths: &Paths, kc: &dyn Keychain) -> Result<()> {
    let settings_path = paths.cs_settings();
    let mut settings = Settings::load(&settings_path).unwrap_or_default();
    if !settings.auto_switch {
        return Ok(());
    }

    let state = State::load(&paths.state_file()).unwrap_or_default();
    let Some(active_name) = state.active.clone() else {
        return Ok(());
    };

    let active_account = keychain::profile_account(&active_name);
    let active_blob = match kc.read(&active_account) {
        Ok(b) => b,
        Err(e) => {
            tracing::info!(profile = %active_name, error = %e, "active profile keychain entry missing");
            return Ok(());
        }
    };
    let active_creds = match OauthCreds::parse(&active_blob) {
        Ok(c) => c,
        Err(_) => {
            tracing::info!(profile = %active_name, "active profile has no OAuth creds");
            return Ok(());
        }
    };

    let active_outcome = match limits::fetch_for(&active_name, &active_creds, paths, CACHE_MAX_AGE)
    {
        Ok(o) => o,
        Err(e) => {
            tracing::info!(profile = %active_name, error = %e, "could not fetch usage for active profile");
            return Ok(());
        }
    };
    // When the live fetch hit 429, fetch_for serves arbitrarily-old past-TTL cache and flags
    // it stale. The endpoint can stay 429-locked for 30+ minutes, so acting on that data could
    // swap credentials based on utilizations that are tens of minutes out of date. Skip this
    // tick rather than switch on data the endpoint refused to refresh.
    if active_outcome.stale {
        tracing::info!(profile = %active_name, "active usage is stale (rate-limited cache); skipping auto-switch this tick");
        return Ok(());
    }
    let active_limits = active_outcome.limits;

    if active_limits.five_hour.utilization < 100.0
        && active_limits.seven_day.utilization < 100.0
    {
        return Ok(());
    }

    let others = enumerate_others(paths, kc, &active_name);

    let decision = auto_switch::decide(&active_limits, &others);
    match decision {
        Decision::Healthy => Ok(()),
        Decision::Switch(target) => switch_under_lock(
            paths,
            kc,
            &mut settings,
            &settings_path,
            &active_name,
            &target,
        ),
        Decision::AllCapped => notify_all_capped(
            &mut settings,
            &settings_path,
            &active_limits,
            &others,
        ),
    }
}

fn enumerate_others(
    paths: &Paths,
    kc: &dyn Keychain,
    active_name: &str,
) -> Vec<(String, UsageLimits)> {
    let mut out = Vec::new();
    let started = Instant::now();
    let mut names: Vec<String> = kc
        .list()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| keychain::parse_profile_name(a).map(|s| s.to_string()))
        .collect();
    names.sort();
    names.dedup();

    for name in names {
        if name == active_name {
            continue;
        }
        if started.elapsed() >= ITERATION_BUDGET {
            tracing::warn!("auto-switch tick exceeded iteration budget; stopping early");
            break;
        }
        let acct = keychain::profile_account(&name);
        let Ok(blob) = kc.read(&acct) else { continue };
        let Ok(creds) = OauthCreds::parse(&blob) else { continue };
        match limits::fetch_for(&name, &creds, paths, CACHE_MAX_AGE) {
            // Same reasoning as the active-profile stale gate: a 429-served past-TTL cache can
            // be tens of minutes old, so a stale candidate must not be trusted as switch target.
            Ok(o) if o.stale => {
                tracing::info!(profile = %name, "skip stale (rate-limited cache) candidate in auto-switch tick");
            }
            Ok(o) => out.push((name, o.limits)),
            Err(e) => tracing::info!(profile = %name, error = %e, "skip profile in auto-switch tick"),
        }
    }
    out
}

fn switch_under_lock(
    paths: &Paths,
    kc: &dyn Keychain,
    settings: &mut Settings,
    settings_path: &std::path::Path,
    expected_active: &str,
    target: &str,
) -> Result<()> {
    let _lock = CsLock::acquire(paths)?;

    // Test-only race injection: simulate the user (or another tick) racing us by
    // overwriting state.active just before we re-check inside the lock.
    if let Ok(injected) = std::env::var("CS_TEST_AUTOSWITCH_PRE_LOCK_STATE_ACTIVE") {
        let mut s = State::load(&paths.state_file()).unwrap_or_default();
        s.active = Some(injected);
        let _ = s.save(&paths.state_file());
    }

    let current = State::load(&paths.state_file()).unwrap_or_default();
    if current.active.as_deref() != Some(expected_active) {
        tracing::info!(
            "active profile changed during auto-switch tick (now {:?}); aborting",
            current.active
        );
        return Ok(());
    }

    crate::commands::switch::run_locked(paths, kc, &GlobalOpts::default(), target)?;
    settings.last_switch_unix = Some(now_unix());
    settings.save(settings_path)?;
    auto_switch::notify_macos(
        "cs auto-switch",
        &format!("Switched to {target} ({expected_active} was at 100%)"),
    );
    Ok(())
}

fn notify_all_capped(
    settings: &mut Settings,
    settings_path: &std::path::Path,
    active: &UsageLimits,
    others: &[(String, UsageLimits)],
) -> Result<()> {
    let now = now_unix();
    let throttled = settings
        .last_capped_notify_unix
        .map(|t| now.saturating_sub(t) < CAPPED_NOTIFY_THROTTLE)
        .unwrap_or(false);
    if throttled {
        return Ok(());
    }
    let earliest = earliest_reset_label(active, others);
    let msg = match earliest {
        Some(s) => format!("All profiles capped — earliest reset at {s}"),
        None => "All profiles capped".to_string(),
    };
    auto_switch::notify_macos("cs auto-switch", &msg);
    settings.last_capped_notify_unix = Some(now);
    settings.save(settings_path)?;
    Ok(())
}

fn earliest_reset_label(active: &UsageLimits, others: &[(String, UsageLimits)]) -> Option<String> {
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut consider = |s: Option<&str>| {
        if let Some(s) = s {
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                let dt = dt.with_timezone(&Utc);
                earliest = Some(match earliest {
                    Some(prev) if prev <= dt => prev,
                    _ => dt,
                });
            }
        }
    };
    for l in std::iter::once(active).chain(others.iter().map(|(_, l)| l)) {
        consider(l.five_hour.resets_at.as_deref());
        consider(l.seven_day.resets_at.as_deref());
    }
    earliest.map(|dt| dt.format("%H:%M").to_string())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
