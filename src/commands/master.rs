use std::fmt;

use crate::cli::{GlobalOpts, MasterArgs};
use crate::error::Result;
use crate::lock::CsLock;
use crate::master;
use crate::output::{emit_json, emit_text};
use crate::paths::Paths;
use crate::state::State;

pub fn run(paths: &Paths, global: &GlobalOpts, args: &MasterArgs) -> Result<()> {
    if args.unset {
        return run_unset(paths, global);
    }
    match &args.name {
        Some(name) => run_set(paths, global, name),
        None => run_status(paths, global),
    }
}

fn run_status(paths: &Paths, global: &GlobalOpts) -> Result<()> {
    let state = State::load(&paths.state_file()).unwrap_or_default();
    let st = master::status(paths, &state)?;
    if global.json {
        emit_json(&st)?;
    } else {
        emit_text(&TextStatus(&st))?;
    }
    Ok(())
}

fn run_set(paths: &Paths, global: &GlobalOpts, name: &str) -> Result<()> {
    // `name` becomes `profile_dir(name)`, the move/symlink target for shared config; an
    // unvalidated `..` would retarget those operations against the wrong directory.
    crate::paths::validate_profile_name(name)?;
    let _lock = CsLock::acquire(paths)?;
    let state_path = paths.state_file();
    let mut state = State::load(&state_path).unwrap_or_default();
    let report = master::set(paths, &mut state, name)?;
    if global.json {
        emit_json(&report)?;
    } else {
        emit_text(&TextSet(&report))?;
    }
    Ok(())
}

fn run_unset(paths: &Paths, global: &GlobalOpts) -> Result<()> {
    let state_path = paths.state_file();
    let mut state = State::load(&state_path).unwrap_or_default();
    if state.master.is_none() {
        if !global.json {
            eprintln!("no master profile designated; nothing to unset");
        } else {
            emit_json(&master::UnsetReport::default())?;
        }
        return Ok(());
    }
    let _lock = CsLock::acquire(paths)?;
    let report = master::unset(paths, &mut state)?;
    if global.json {
        emit_json(&report)?;
    } else {
        emit_text(&TextUnset(&report))?;
    }
    Ok(())
}

struct TextSet<'a>(&'a master::SetReport);
impl fmt::Display for TextSet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = self.0;
        match &r.previous_master {
            Some(prev) if prev != &r.master => {
                writeln!(f, "master: {} -> {}", prev, r.master)?;
            }
            _ => writeln!(f, "master: {}", r.master)?,
        }
        if !r.moved.is_empty() {
            writeln!(f, "moved:")?;
            for n in &r.moved {
                writeln!(f, "  {n}")?;
            }
        }
        if !r.already.is_empty() {
            writeln!(f, "already symlinked: {}", r.already.join(", "))?;
        }
        if !r.skipped_empty.is_empty() {
            writeln!(f, "skipped (missing/empty): {}", r.skipped_empty.join(", "))?;
        }
        Ok(())
    }
}

struct TextUnset<'a>(&'a master::UnsetReport);
impl fmt::Display for TextUnset<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = self.0;
        if let Some(prev) = &r.previous_master {
            writeln!(f, "cleared master: {prev}")?;
        }
        if !r.restored.is_empty() {
            writeln!(f, "restored to ~/.claude:")?;
            for n in &r.restored {
                writeln!(f, "  {n}")?;
            }
        }
        Ok(())
    }
}

struct TextStatus<'a>(&'a master::MasterStatus);
impl fmt::Display for TextStatus<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.master {
            Some(name) => writeln!(f, "master: {name}")?,
            None => writeln!(f, "master: (none designated)")?,
        }
        if let Some(dir) = &self.0.master_dir {
            writeln!(f, "  dir: {}", dir.display())?;
        }
        for item in &self.0.items {
            writeln!(
                f,
                "  {:<12} {:?}  {}",
                item.name,
                item.state,
                item.claude_path.display()
            )?;
        }
        Ok(())
    }
}
