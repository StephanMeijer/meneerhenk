//! What running sessions say, as they say it (#238). A session's
//! conversation is stored only when it ends; until then the messages it
//! appends wait here, per session, for the dashboard's live view of a lane.
//!
//! Kept apart from the feed's replay of changes ([`crate::live`]): one
//! message per tool round would flush that replay for every other
//! follower. Each session keeps its newest messages up to a cap, and its
//! buffer goes when the session ends; the stored transcript is the record.
//! What is here is the conversation before compaction, so it can show
//! more than the stored transcript of the same session.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use henk_domain::run::RunId;
use serde_json::Value;
use tokio::sync::broadcast;

/// How many messages of one session wait at most.
const MOST_MESSAGES: usize = 200;
/// How many bytes of one session's messages wait at most.
const MOST_BYTES: usize = 512 * 1024;
/// The longest text of one message part the live view keeps; the rest is
/// cut with a note. The stored transcript keeps it whole.
const MOST_TEXT: usize = 16 * 1024;
/// How many messages a slow follower may fall behind before it is
/// dropped and reconnects.
const CHANNEL: usize = 256;

/// One message a session appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// Its place in the session, from 1.
    pub seq: u64,
    /// The turn it belongs to; 0 for the opening.
    pub turn: u32,
    /// When it was appended, RFC 3339.
    pub at: String,
    /// The message as JSON, as a stored transcript holds it.
    pub body: String,
}

/// What happened to a session, for its followers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum News {
    /// It appended a message.
    Said {
        /// The run.
        run: RunId,
        /// The session.
        session: String,
        /// The message.
        said: Arc<Said>,
    },
    /// It ended, or its run did; its transcript is stored by now.
    Ended {
        /// The run.
        run: RunId,
        /// The session; `None` for every session of the run.
        session: Option<String>,
    },
}

impl News {
    /// Whether this is news of `session` of `run`.
    #[must_use]
    pub fn is_of(&self, of_run: &RunId, of_session: &str) -> bool {
        match self {
            Self::Said { run, session, .. } => run == of_run && session == of_session,
            Self::Ended { run, session } => {
                run == of_run && session.as_deref().is_none_or(|s| s == of_session)
            }
        }
    }
}

/// What one session has said, as far back as the caps allow.
#[derive(Debug, Default)]
struct Log {
    seq: u64,
    said: VecDeque<Arc<Said>>,
    bytes: usize,
    /// Older messages were let go to stay under the caps.
    cut: bool,
}

/// The running sessions of this process and what they said.
#[derive(Debug)]
pub struct Sessions {
    logs: Mutex<HashMap<(RunId, String), Log>>,
    sender: broadcast::Sender<Arc<News>>,
}

impl Default for Sessions {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(CHANNEL);
        Self {
            logs: Mutex::new(HashMap::new()),
            sender,
        }
    }
}

/// A session's messages to catch up with: these, and whether older ones
/// were let go.
pub type Snapshot = (Vec<Arc<Said>>, bool);

impl Sessions {
    /// Keeps a message `session` of `run` appended, and tells followers.
    pub fn said(&self, run: &RunId, session: &str, turn: u32, body: &str, at: String) {
        let body = within_caps(body);
        let mut logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        let log = logs.entry((run.clone(), session.to_owned())).or_default();
        log.seq += 1;
        let said = Arc::new(Said {
            seq: log.seq,
            turn,
            at,
            body,
        });
        log.bytes += said.body.len();
        log.said.push_back(Arc::clone(&said));
        while log.said.len() > MOST_MESSAGES || (log.bytes > MOST_BYTES && log.said.len() > 1) {
            if let Some(gone) = log.said.pop_front() {
                log.bytes -= gone.body.len();
                log.cut = true;
            }
        }
        // Sent under the lock, so followers see a session's messages in
        // order. No follower is no error.
        let _ = self.sender.send(Arc::new(News::Said {
            run: run.clone(),
            session: session.to_owned(),
            said,
        }));
    }

    /// `session` of `run` ended: its messages go, its followers are told.
    pub fn ended(&self, run: &RunId, session: &str) {
        let mut logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        logs.remove(&(run.clone(), session.to_owned()));
        let _ = self.sender.send(Arc::new(News::Ended {
            run: run.clone(),
            session: Some(session.to_owned()),
        }));
    }

    /// Every session of `run` ended, as when the run did.
    pub fn run_ended(&self, run: &RunId) {
        let mut logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        logs.retain(|(of, _), _| of != run);
        let _ = self.sender.send(Arc::new(News::Ended {
            run: run.clone(),
            session: None,
        }));
    }

    /// Everything from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<News>> {
        self.sender.subscribe()
    }

    /// The messages of `session` after `seq`, when they are all still
    /// here; `None` when they are not, or the session is not running here.
    #[must_use]
    pub fn since(&self, run: &RunId, session: &str, seq: u64) -> Option<Vec<Arc<Said>>> {
        let logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        let log = logs.get(&(run.clone(), session.to_owned()))?;
        let oldest = log.said.front().map_or(log.seq + 1, |s| s.seq);
        if seq > log.seq || seq + 1 < oldest {
            return None;
        }
        Some(log.said.iter().filter(|s| s.seq > seq).cloned().collect())
    }

    /// What `session` of `run` said as far back as is kept; `None` when it
    /// is not running in this process.
    #[must_use]
    pub fn snapshot(&self, run: &RunId, session: &str) -> Option<Snapshot> {
        let logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        let log = logs.get(&(run.clone(), session.to_owned()))?;
        Some((log.said.iter().cloned().collect(), log.cut))
    }

    /// Whether `session` of `run` has said anything in this process and
    /// not ended.
    #[must_use]
    pub fn is_live(&self, run: &RunId, session: &str) -> bool {
        self.logs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&(run.clone(), session.to_owned()))
    }
}

/// The message with every text longer than [`MOST_TEXT`] cut, with a
/// note; as it was when nothing is that long or it is not JSON.
fn within_caps(body: &str) -> String {
    if body.len() <= MOST_TEXT {
        return body.to_owned();
    }
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    cut_texts(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| body.to_owned())
}

fn cut_texts(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > MOST_TEXT => {
            let mut end = MOST_TEXT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let rest = text.len() - end;
            text.truncate(end);
            let _ = write!(
                text,
                "\n[{rest} more bytes; the transcript has them once the session ends]"
            );
        }
        Value::Array(items) => items.iter_mut().for_each(cut_texts),
        Value::Object(fields) => fields.values_mut().for_each(cut_texts),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn run() -> RunId {
        RunId::parse("r-1").unwrap()
    }

    fn say(sessions: &Sessions, session: &str, text: &str) {
        let body = serde_json::json!({ "role": "assistant", "blocks": [{ "text": text }] });
        sessions.said(
            &run(),
            session,
            1,
            &body.to_string(),
            "2026-10-08T12:00:00Z".into(),
        );
    }

    #[test]
    fn a_session_replays_after_a_seq_while_it_holds_everything_since() {
        let sessions = Sessions::default();
        for text in ["one", "two", "three"] {
            say(&sessions, "lane-a", text);
        }
        say(&sessions, "lane-b", "other");
        let after = sessions.since(&run(), "lane-a", 1).unwrap();
        assert_eq!(after.iter().map(|s| s.seq).collect::<Vec<_>>(), [2, 3]);
        assert!(sessions.since(&run(), "lane-a", 3).unwrap().is_empty());
        assert!(
            sessions.since(&run(), "lane-a", 4).is_none(),
            "a seq it never gave"
        );
        assert!(
            sessions.since(&run(), "lane-c", 0).is_none(),
            "not running here"
        );
        let (all, cut) = sessions.snapshot(&run(), "lane-b").unwrap();
        assert_eq!((all.len(), cut), (1, false));
    }

    #[test]
    fn a_long_session_keeps_its_newest_messages_and_says_it_let_older_go() {
        let sessions = Sessions::default();
        for n in 0..MOST_MESSAGES + 5 {
            say(&sessions, "lane-a", &format!("message {n}"));
        }
        let (kept, cut) = sessions.snapshot(&run(), "lane-a").unwrap();
        assert!(cut);
        assert_eq!(kept.len(), MOST_MESSAGES);
        assert_eq!(kept[0].seq, 6);
        assert!(sessions.since(&run(), "lane-a", 1).is_none(), "gone");
        assert_eq!(
            sessions.since(&run(), "lane-a", 5).unwrap().len(),
            MOST_MESSAGES
        );

        let big = Sessions::default();
        let text = "x".repeat(MOST_TEXT - 100);
        for _ in 0..(MOST_BYTES / MOST_TEXT) + 4 {
            say(&big, "lane-a", &text);
        }
        let (kept, cut) = big.snapshot(&run(), "lane-a").unwrap();
        assert!(cut);
        assert!(kept.iter().map(|s| s.body.len()).sum::<usize>() <= MOST_BYTES);
    }

    #[test]
    fn a_long_text_is_cut_with_a_note_and_stays_json() {
        let sessions = Sessions::default();
        say(&sessions, "lane-a", &"é".repeat(MOST_TEXT));
        let (kept, _) = sessions.snapshot(&run(), "lane-a").unwrap();
        let value: Value = serde_json::from_str(&kept[0].body).unwrap();
        let text = value["blocks"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("éé"));
        assert!(text.ends_with("more bytes; the transcript has them once the session ends]"));
        assert!(text.len() < MOST_TEXT + 100);
    }

    #[test]
    fn an_ended_session_lets_its_messages_go_and_tells_its_followers() {
        let sessions = Sessions::default();
        let mut news = sessions.subscribe();
        say(&sessions, "lane-a", "hello");
        say(&sessions, "lane-b", "hello");
        sessions.ended(&run(), "lane-a");
        assert!(!sessions.is_live(&run(), "lane-a"));
        assert!(sessions.is_live(&run(), "lane-b"));
        let first = news.try_recv().unwrap();
        assert!(first.is_of(&run(), "lane-a") && !first.is_of(&run(), "lane-b"));
        let _ = news.try_recv().unwrap();
        let ended = news.try_recv().unwrap();
        assert_eq!(
            *ended,
            News::Ended {
                run: run(),
                session: Some("lane-a".into())
            }
        );
        sessions.run_ended(&run());
        assert!(!sessions.is_live(&run(), "lane-b"));
        let all = news.try_recv().unwrap();
        assert!(
            all.is_of(&run(), "lane-b"),
            "a run's end ends every session"
        );
    }
}
