use std::fs;

use crate::cli::{GlobalOpts, RenameArgs};
use crate::error::{Error, Result};
use crate::keychain::{self, Keychain};
use crate::lock::CsLock;
use crate::master;
use crate::paths::Paths;
use crate::state::State;

pub fn run(
    paths: &Paths,
    kc: &dyn Keychain,
    _global: &GlobalOpts,
    args: &RenameArgs,
) -> Result<()> {
    if args.from == args.to {
        return Err(Error::InvalidArgument(
            "source and target are the same".into(),
        ));
    }
    // Validate BOTH names: `from` feeds `profile_dir`/`fs::rename` just like `to`, so an
    // unvalidated `../x` source is as dangerous as the destination.
    crate::paths::validate_profile_name(&args.from)?;
    crate::paths::validate_profile_name(&args.to)?;

    let _lock = CsLock::acquire(paths)?;

    let from_acct = keychain::profile_account(&args.from);
    let to_acct = keychain::profile_account(&args.to);

    let blob = kc.read(&from_acct).ok();
    let from_dir = paths.profile_dir(&args.from);
    let to_dir = paths.profile_dir(&args.to);
    let dir_exists = from_dir.exists();
    if blob.is_none() && !dir_exists {
        return Err(Error::ProfileNotFound(args.from.clone()));
    }
    if blob.is_some() && kc.read(&to_acct).is_ok() {
        return Err(Error::ProfileExists(args.to.clone()));
    }
    if dir_exists && to_dir.exists() {
        return Err(Error::ProfileExists(args.to.clone()));
    }

    let state_path = paths.state_file();
    let mut state = State::load(&state_path).unwrap_or_default();
    let was_master = state.master.as_deref() == Some(args.from.as_str());

    if let Some(blob) = blob.as_ref() {
        keychain::write_verified(kc, &to_acct, blob)?;
        kc.delete(&from_acct)?;
    }
    if dir_exists {
        if let Some(parent) = to_dir.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
        }
        fs::rename(&from_dir, &to_dir).map_err(|e| Error::io_at(&to_dir, e))?;
        // Repoint the live ~/.claude shared symlinks into the renamed dir immediately — they
        // currently dangle into the old (now-moved) path. Doing this before the state write
        // bounds the dangling window to a single fs op; if interrupted here, state.master is
        // still `from`, which is a detectable/repairable state rather than a silent orphan.
        if was_master {
            master::retarget_symlinks(paths, &args.to)?;
        }
    }

    let mut changed = false;
    for slot in [&mut state.active, &mut state.previous, &mut state.default] {
        if slot.as_deref() == Some(&args.from) {
            *slot = Some(args.to.clone());
            changed = true;
        }
    }
    if was_master {
        state.master = Some(args.to.clone());
        changed = true;
    }
    if changed {
        state.save(&state_path)?;
    }

    eprintln!("renamed `{}` -> `{}`", args.from, args.to);
    Ok(())
}
