//! League providers + enrichment + pipeline (Python `providers/espn.py`, `fantrax*.py`, `enrich.py`,
//! `*_enrich.py`, `providers/__init__.py`, the cli/web load pipeline). Owner of this file: W5-provider-pipeline.

pub mod deployment_enrich;
pub mod enrich;
pub mod espn;
pub mod fantrax;
pub mod fantrax_auth;
pub mod fantrax_crosscheck;
pub mod fantrax_official;
pub mod lines_enrich;
pub mod pipeline;
pub mod preseason_enrich;
pub mod registry;
pub mod rookie_enrich;
pub mod snapshot;
pub mod xg_enrich;
