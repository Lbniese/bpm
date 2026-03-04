//! `bpm doctor` orchestration.

use std::env;

use bpm::doctor::run as doctor_run;

pub(super) fn run(json: bool) -> anyhow::Result<()> {
    let report = doctor_run(&env::current_dir()?);
    if json {
        println!("{}", report.render_json());
    } else {
        print!("{}", report.render_text());
    }
    if report.has_error() {
        anyhow::bail!("doctor reported one or more errors");
    }
    Ok(())
}
