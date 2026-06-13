use crate::cli::{DoctorArgs, GlobalOpts};
use crate::doctor;
use crate::error::Result;
use crate::keychain::Keychain;
use crate::output::{emit, OutputOpts};
use crate::paths::Paths;

pub fn run(
    paths: &Paths,
    kc: &dyn Keychain,
    global: &GlobalOpts,
    _args: &DoctorArgs,
) -> Result<()> {
    let report = doctor::run(paths, kc)?;
    emit(OutputOpts { json: global.json }, &report)
}
