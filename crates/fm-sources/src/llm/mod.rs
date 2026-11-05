//! Rust port of `fantasy_manager/llm/` (OpenRouter client, narration, `fm ask`, prompt context).
//! Owner: work package `W2-news-odds-llm`. The LLM provider is OpenRouter (default model
//! `openrouter/free`, free-only guard) - never the Claude API (user decision).

pub mod context;
pub mod openrouter;

pub use openrouter::{LLMClient, LLMError};

/// Keyword arguments of `complete(system, user, max_tokens=800, temperature=0.3, json_mode=False,
/// min_chars=0)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompleteOpts {
    pub max_tokens: i64,
    pub temperature: f64,
    pub json_mode: bool,
    pub min_chars: usize,
}

impl Default for CompleteOpts {
    fn default() -> Self {
        CompleteOpts { max_tokens: 800, temperature: 0.3, json_mode: false, min_chars: 0 }
    }
}

/// `SupportsComplete` protocol (`LLMClient` or a test stub). `model` is `getattr(llm, "model",
/// None)`. Errors are `anyhow` so a stub can raise anything; `LLMClient` raises [`LLMError`].
pub trait SupportsComplete {
    fn available(&self) -> bool;
    fn model(&self) -> Option<String> {
        None
    }
    fn complete(&self, system: &str, user: &str, opts: CompleteOpts) -> anyhow::Result<String>;
}
