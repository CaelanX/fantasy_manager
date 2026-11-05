//! The `fm` binary (Python `cli.py`, `cli_harness.py`, `cli_backtest.py`).
//! Owner of main.rs/app.rs/common.rs: W9-cli-core.

pub mod app;
pub mod cmd_admin;
pub mod cmd_analysis;
pub mod cmd_backtest;
pub mod cmd_harness;
pub mod cmd_league;
pub mod cmd_recs;
pub mod common;

fn main() -> anyhow::Result<()> {
    app::run()
}
