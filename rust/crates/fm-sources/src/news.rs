//! Rust port of `fantasy_manager/providers/news.py`.
//! Owner: work package `W2-news-odds-llm` (wave 2). Status: TODO - only `NewsItem` is written
//! (by the architect, because W2-nhl-matching's news_match uses it in the same wave).
//! See rust/WORK_PACKAGES.json for the package notes and rust/ARCHITECTURE.md for conventions.

use fm_core::PyDateTime;
use serde::{Deserialize, Serialize};

/// `providers.news.NewsItem` (pydantic). `published` is timezone-aware UTC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewsItem {
    /// "rotowire" | "espn"
    pub source: String,
    /// feed guid (for dedupe)
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub player_name: Option<String>,
    pub headline: String,
    #[serde(default)]
    pub blurb: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub published: Option<PyDateTime>,
    #[serde(default)]
    pub tags: Vec<String>,
}
