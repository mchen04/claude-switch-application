use crate::cli::{GlobalOpts, NameArg};
use crate::error::{Error, Result};
use crate::keychain::{self, Keychain};
use crate::lock::CsLock;
use crate::paths::Paths;
use crate::state::State;

pub fn set(paths: &Paths, kc: &dyn Keychain, _global: &GlobalOpts, args: &NameArg) -> Result<()> {
    crate::paths::validate_profile_name(&args.name)?;
    let target = keychain::profile_account(&args.name);
    if kc.read(&target).is_err() {
        return Err(Error::ProfileNotFound(args.name.clone()));
    }
    let _lock = CsLock::acquire(paths)?;
    let path = paths.state_file();
    let mut state = State::load(&path).unwrap_or_default();
    state.default = Some(args.name.clone());
    state.save(&path)?;
    eprintln!("default profile set to `{}`", args.name);
    Ok(())
}

pub fn go(paths: &Paths, kc: &dyn Keychain, global: &GlobalOpts) -> Result<()> {
    let state = State::load(&paths.state_file()).unwrap_or_default();
    let name = state.default.clone().ok_or_else(|| {
        Error::Other("no default profile set (run `cs default <name>` first)".into())
    })?;
    super::switch::run(paths, kc, global, &name, &[])
}
