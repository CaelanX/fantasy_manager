//! Data source clients and parsers (Python `providers/{nhl,moneypuck,dailyfaceoff,injuries,news,news_roles,odds}.py`,
//! `matching/{matcher,crosswalk}.py`, `report/news_match.py`, `llm/*`). Module tree by the architect.

pub mod dailyfaceoff;
pub mod injuries;
pub mod llm;
pub mod matching;
pub mod moneypuck;
pub mod news;
pub mod news_match;
pub mod news_roles;
pub mod nhl;
pub mod odds;
