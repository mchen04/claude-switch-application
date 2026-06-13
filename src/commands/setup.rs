use std::fs;

use crate::cli::{GlobalOpts, SetupArgs};
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::shell::{self, Shell};

pub fn run(_paths: &Paths, _global: &GlobalOpts, args: &SetupArgs) -> Result<()> {
    let shell = Shell::detect(args.shell)?;
    let rc = shell
        .rc_path()
        .ok_or_else(|| Error::Config("HOME unset".into()))?;
    let existing = match fs::read_to_string(&rc) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::io_at(&rc, e)),
    };
    let updated = shell::upsert_block(&existing, shell.snippet())?;

    if existing == updated {
        eprintln!(
            "{} already contains the cs wrapper; nothing to do",
            rc.display()
        );
        return Ok(());
    }
    crate::jsonio::atomic_write_bytes(&rc, updated.as_bytes())?;
    eprintln!("installed cs wrapper into {}", rc.display());
    eprintln!(
        "restart your shell or `source {}` to activate",
        rc.display()
    );
    Ok(())
}
