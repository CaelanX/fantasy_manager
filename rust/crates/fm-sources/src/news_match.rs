//! Rust port of `fantasy_manager/report/news_match.py`.
//! Owner: work package `W2-nhl-matching` (wave 2). Status: TODO - not yet translated.
//! The signatures below are fixed by the architect (W2-news-odds-llm calls them in the same wave).

use fm_core::{Map, Player};

use crate::news::NewsItem;

/// `match_news_to_players(news, players)` -> cid -> items (insertion order = Python dict order).
pub fn match_news_to_players(_news: &[NewsItem], _players: &[&Player]) -> Map<String, Vec<NewsItem>> {
    todo!("W2-nhl-matching")
}

/// `player_news_summary(items, limit=3, blurb_chars=200)`.
pub fn player_news_summary(_items: &[NewsItem], _limit: usize, _blurb_chars: usize) -> Vec<String> {
    todo!("W2-nhl-matching")
}
