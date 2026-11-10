//! Rust port of `fantasy_manager/llm/` (OpenRouter client, narration, `fm ask`, prompt context).
//! Owner: work package `W2-news-odds-llm`. The LLM provider is OpenRouter (default model
//! `openrouter/free`, free-only guard) - never the Claude API (user decision) - or the
//! OpenAI-compatible server in `FM_LLM_BASE_URL` (a model on the user's own PC).

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
    /// `complete`, handing the reply over as it is written; returns the whole answer. `on`
    /// returns false to stop early (the reader went away). The default sends it in one piece.
    fn complete_stream(&self, system: &str, user: &str, opts: CompleteOpts, on: &mut dyn FnMut(StreamPart) -> bool) -> anyhow::Result<String> {
        let text = self.complete(system, user, opts)?;
        on(StreamPart::Answer(&text));
        Ok(text)
    }
}

/// A piece of a streamed reply: the model's reasoning before the answer, or the answer itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StreamPart<'a> {
    Thinking(&'a str),
    Answer(&'a str),
}
