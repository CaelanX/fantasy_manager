//! The Ask tab's saved chats (`<FM_DATA_DIR>/chats.json`): per league, newest first. Same store
//! rules as the watchlist: reads never fail (missing / corrupt file = empty), writes are atomic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub const CHATS_FILE: &str = "chats.json";
/// chats kept per league; the least recently used go first
pub const MAX_CHATS: usize = 40;
pub const TITLE_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub question: String,
    pub answer: String,
    #[serde(default)]
    pub model: Option<String>,
    /// local time, ISO
    pub at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub updated: String,
    /// cids the chat was opened about
    #[serde(default)]
    pub focus: Vec<String>,
    /// text pasted into the chat
    #[serde(default)]
    pub pasted: String,
    /// a question asked but not answered yet (the page streams its answer)
    #[serde(default)]
    pub pending: Option<String>,
    pub turns: Vec<Turn>,
}

type Store = BTreeMap<String, Vec<Chat>>;

static LOCK: Mutex<()> = Mutex::new(());

pub fn chats_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CHATS_FILE)
}

fn load_store(data_dir: &Path) -> Store {
    std::fs::read_to_string(chats_path(data_dir)).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default()
}

/// A league's chats, most recently used first.
pub fn load(data_dir: &Path, league: &str) -> Vec<Chat> {
    load_store(data_dir).remove(league).unwrap_or_default()
}

fn update<T>(data_dir: &Path, league: &str, f: impl FnOnce(&mut Vec<Chat>) -> T) -> anyhow::Result<T> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_store(data_dir);
    let list = store.entry(league.to_string()).or_default();
    let out = f(list);
    list.truncate(MAX_CHATS);
    if list.is_empty() {
        store.remove(league);
    }
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = chats_path(data_dir);
    let tmp = data_dir.join(format!(".chats-{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&store)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(out)
}

pub fn title_from(question: &str) -> String {
    let q = question.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.chars().count() <= TITLE_CHARS {
        q
    } else {
        format!("{}…", q.chars().take(TITLE_CHARS - 1).collect::<String>().trim_end())
    }
}

/// Put `question` to chat `id` as its pending question, or start a new chat with it when `id` is
/// None or no longer exists (`focus` and `pasted` are stored on a new chat only). The chat moves
/// to the top; returns its id. `at` is local time, ISO.
pub fn ask(data_dir: &Path, league: &str, id: Option<&str>, focus: &[String], pasted: &str, question: &str, at: &str) -> anyhow::Result<String> {
    update(data_dir, league, |list| {
        let mut chat = match id.and_then(|id| list.iter().position(|c| c.id == id)) {
            Some(i) => list.remove(i),
            None => {
                // the start time, made unique within the league
                let base: String = at.chars().filter(char::is_ascii_digit).collect();
                let mut new_id = base.clone();
                let mut n = 1;
                while list.iter().any(|c| c.id == new_id) {
                    n += 1;
                    new_id = format!("{base}-{n}");
                }
                Chat { id: new_id, title: title_from(question), updated: String::new(), focus: focus.to_vec(), pasted: pasted.to_string(), pending: None, turns: vec![] }
            }
        };
        chat.updated = at.to_string();
        chat.pending = Some(question.to_string());
        let id = chat.id.clone();
        list.insert(0, chat);
        id
    })
}

/// The answer to a chat's pending question arrived: record the turn.
pub fn answer(data_dir: &Path, league: &str, id: &str, turn: Turn) -> anyhow::Result<()> {
    update(data_dir, league, |list| {
        if let Some(c) = list.iter_mut().find(|c| c.id == id) {
            c.pending = None;
            c.updated = turn.at.clone();
            c.turns.push(turn);
        }
    })
}

/// Drop a chat's pending question; a chat with no answered question goes with it.
pub fn discard(data_dir: &Path, league: &str, id: &str) -> anyhow::Result<()> {
    update(data_dir, league, |list| {
        if let Some(c) = list.iter_mut().find(|c| c.id == id) {
            c.pending = None;
        }
        list.retain(|c| c.id != id || !c.turns.is_empty());
    })
}

/// Chats whose answer is being written right now, so a reload does not start a second run.
static WRITING: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub struct Writing(String);

/// Claim the chat for one writer (None: another request is already answering it).
pub fn claim(data_dir: &Path, league: &str, id: &str) -> Option<Writing> {
    let key = format!("{}|{league}|{id}", data_dir.display());
    let mut w = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if w.contains(&key) {
        return None;
    }
    w.push(key.clone());
    Some(Writing(key))
}

impl Drop for Writing {
    fn drop(&mut self) {
        WRITING.lock().unwrap_or_else(|e| e.into_inner()).retain(|k| *k != self.0);
    }
}

pub fn rename(data_dir: &Path, league: &str, id: &str, title: &str) -> anyhow::Result<()> {
    let title = title_from(title);
    update(data_dir, league, |list| {
        if let Some(c) = list.iter_mut().find(|c| c.id == id).filter(|_| !title.is_empty()) {
            c.title = title;
        }
    })
}

pub fn delete(data_dir: &Path, league: &str, id: &str) -> anyhow::Result<()> {
    update(data_dir, league, |list| list.retain(|c| c.id != id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(q: &str, at: &str) -> Turn {
        Turn { question: q.into(), answer: "a".into(), model: None, at: at.into() }
    }

    #[test]
    fn chats_round_trip_newest_first() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        assert!(load(d, "espn").is_empty());
        let a = ask(d, "espn", None, &["espn:1".into()], "an offer", "First   question?", "2026-11-10T09:00:00").unwrap();
        let b = ask(d, "espn", None, &[], "", &"x".repeat(80), "2026-11-10T09:00:00").unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("20261110090000", "20261110090000-2"));
        assert_eq!(load(d, "espn")[1].pending.as_deref(), Some("First   question?"));
        answer(d, "espn", &a, turn("First   question?", "2026-11-10T09:01:00")).unwrap();
        answer(d, "espn", &b, turn("x", "2026-11-10T09:01:00")).unwrap();
        // a follow-up moves its chat to the top and keeps what the chat was opened with
        assert_eq!(ask(d, "espn", Some(&a), &[], "ignored", "And then?", "2026-11-10T09:04:00").unwrap(), a);
        answer(d, "espn", &a, turn("And then?", "2026-11-10T09:05:00")).unwrap();
        // an unanswered question can be dropped; a chat that never got an answer goes with it
        ask(d, "espn", Some(&a), &[], "", "Never mind", "2026-11-10T09:05:00").unwrap();
        let c = ask(d, "espn", None, &[], "", "Empty", "2026-11-10T09:06:00").unwrap();
        discard(d, "espn", &a).unwrap();
        discard(d, "espn", &c).unwrap();
        let held = claim(d, "espn", &a).unwrap();
        assert!(claim(d, "espn", &a).is_none() && claim(d, "espn", &b).is_some());
        drop(held);
        assert!(claim(d, "espn", &a).is_some());
        let list = load(d, "espn");
        assert!(list[0].pending.is_none());
        assert_eq!(list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec![a.as_str(), b.as_str()]);
        assert_eq!((list[0].title.as_str(), list[0].turns.len(), list[0].pasted.as_str()), ("First question?", 2, "an offer"));
        assert_eq!((list[0].focus.clone(), list[0].updated.as_str()), (vec!["espn:1".to_string()], "2026-11-10T09:05:00"));
        assert!(list[1].title.chars().count() == TITLE_CHARS && list[1].title.ends_with('…'));
        assert!(load(d, "fantrax").is_empty());

        rename(d, "espn", &a, "  Makar   trade ").unwrap();
        rename(d, "espn", &b, "   ").unwrap();
        delete(d, "espn", "nope").unwrap();
        let list = load(d, "espn");
        assert_eq!(list[0].title, "Makar trade");
        assert_eq!(list[1].title.chars().count(), TITLE_CHARS, "a blank title is ignored");
        delete(d, "espn", &a).unwrap();
        delete(d, "espn", &b).unwrap();
        assert_eq!(std::fs::read_to_string(chats_path(d)).unwrap(), "{}\n");
        std::fs::write(chats_path(d), "not json").unwrap();
        assert!(load(d, "espn").is_empty());
    }
}
