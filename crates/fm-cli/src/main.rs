//! The `fm` binary (Python `cli.py`, `cli_harness.py`, `cli_backtest.py`).
//! Owner of main.rs/app.rs/common.rs: W9-cli-core. Commands register themselves into
//! `app::COMMANDS` (see app.rs); a new command module only needs its `pub mod` line here.

pub mod app;
pub mod cmd_admin;
pub mod cmd_analysis;
pub mod cmd_backtest;
pub mod cmd_harness;
pub mod cmd_keepers;
pub mod cmd_league;
pub mod cmd_recs;
pub mod common;

fn main() {
    let code = app::run();
    std::process::exit(code);
}
