//! Ordered application of durable Session windows and live event frames.

use serde_json::Value;

use crate::client::{ClientError, ClientResult};

/// Change produced by one durable Session frame.
#[derive(Clone, Debug, PartialEq)]
pub enum JournalUpdate {
    /// Replace the visible history window. Older pages may still be available.
    Replace(Vec<Value>),
    /// Append newly delivered live events. Duplicates produce an empty vector.
    Append(Vec<Value>),
}

impl JournalUpdate {
    #[must_use]
    pub fn into_events(self) -> Vec<Value> {
        match self {
            Self::Replace(events) | Self::Append(events) => events,
        }
    }
}

/// Tracks one Session's visible history window and durable live cursor.
///
/// Follow snapshots replace the window, including on reconnect. Callers use
/// `has_more` and `prepend_page` to load older history; live event sequences
/// advance independently. Assistant-stream frames are transient and never
/// advance the durable cursor.
#[derive(Clone, Debug)]
pub struct SessionJournal {
    session_id: String,
    cursor: Option<i64>,
    window_start: Option<i64>,
    has_more: bool,
}

impl SessionJournal {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cursor: None,
            window_start: None,
            has_more: false,
        }
    }

    /// Last durable sequence applied, or `None` before the first snapshot.
    #[must_use]
    pub fn cursor(&self) -> Option<i64> {
        self.cursor
    }

    /// First sequence in the current visible window.
    #[must_use]
    pub fn window_start(&self) -> Option<i64> {
        self.window_start
    }

    /// Whether `session/page` can provide history older than this window.
    #[must_use]
    pub fn has_more(&self) -> bool {
        self.has_more
    }

    /// Apply one `session/follow` frame.
    pub fn apply(&mut self, frame: &Value) -> ClientResult<JournalUpdate> {
        match frame.get("type").and_then(Value::as_str) {
            Some("snapshot") => self.apply_snapshot(frame),
            Some("event") => self.apply_event(frame).map(JournalUpdate::Append),
            Some("assistant-stream") if self.cursor.is_some() => {
                Ok(JournalUpdate::Append(Vec::new()))
            }
            Some("assistant-stream") => Err(ClientError::Protocol(
                "assistant stream arrived before the initial snapshot",
            )),
            _ => Err(ClientError::Protocol("unknown Session journal frame")),
        }
    }

    /// Prepend one older `session/page` result to the current window.
    pub fn prepend_page(&mut self, page: &Value) -> ClientResult<Vec<Value>> {
        if self.cursor.is_none() || !self.has_more {
            return Err(ClientError::Protocol(
                "Session page arrived without an older-history window",
            ));
        }
        let parsed = parse_page(page)?;
        if self.cursor != Some(parsed.cursor) {
            return Err(ClientError::Protocol(
                "Session page used a different history cutoff",
            ));
        }
        let meets_window = match parsed.events.last() {
            Some((seq, _)) => self.window_start == seq.checked_add(1),
            None => self.window_start == Some(0) && !parsed.has_more,
        };
        if !meets_window {
            return Err(ClientError::Protocol(
                "Session page does not meet the current window",
            ));
        }
        self.window_start = parsed
            .events
            .first()
            .map(|(seq, _)| *seq)
            .or(self.window_start);
        self.has_more = parsed.has_more;
        Ok(parsed.events.into_iter().map(|(_, event)| event).collect())
    }

    fn apply_snapshot(&mut self, snapshot: &Value) -> ClientResult<JournalUpdate> {
        if snapshot.pointer("/header/id").and_then(Value::as_str) != Some(&self.session_id) {
            return Err(ClientError::Protocol("Session snapshot has the wrong id"));
        }
        let parsed = parse_page(snapshot)?;
        let valid_tail = match parsed.events.last() {
            Some((seq, _)) => *seq == parsed.cursor,
            None => parsed.cursor == -1,
        };
        if !valid_tail {
            return Err(ClientError::Protocol(
                "Session snapshot records do not meet its cursor",
            ));
        }
        if self.cursor.is_some_and(|cursor| parsed.cursor < cursor) {
            return Err(ClientError::Protocol(
                "Session snapshot moved the cursor back",
            ));
        }
        self.cursor = Some(parsed.cursor);
        self.window_start = parsed.events.first().map(|(seq, _)| *seq);
        self.has_more = parsed.has_more;
        Ok(JournalUpdate::Replace(
            parsed.events.into_iter().map(|(_, event)| event).collect(),
        ))
    }

    fn apply_event(&mut self, frame: &Value) -> ClientResult<Vec<Value>> {
        let cursor = self.cursor.ok_or(ClientError::Protocol(
            "Session event arrived before the initial snapshot",
        ))?;
        let event =
            frame
                .get("event")
                .filter(|event| event.is_object())
                .ok_or(ClientError::Protocol(
                    "Session event frame omitted its event",
                ))?;
        let seq = event_seq(event)?;
        if seq <= cursor {
            return Ok(Vec::new());
        }
        if Some(cursor) != seq.checked_sub(1) {
            return Err(ClientError::Protocol(
                "Session event stream skipped a sequence",
            ));
        }
        self.cursor = Some(seq);
        self.window_start.get_or_insert(seq);
        Ok(vec![event.clone()])
    }
}

struct ParsedPage {
    cursor: i64,
    events: Vec<(i64, Value)>,
    has_more: bool,
}

fn parse_page(page: &Value) -> ClientResult<ParsedPage> {
    let cursor = page
        .get("cursor")
        .and_then(Value::as_i64)
        .filter(|cursor| *cursor >= -1)
        .ok_or(ClientError::Protocol("Session page omitted its cursor"))?;
    let has_more = page
        .get("hasMore")
        .and_then(Value::as_bool)
        .ok_or(ClientError::Protocol("Session page omitted hasMore"))?;
    let records = page
        .get("records")
        .and_then(Value::as_array)
        .ok_or(ClientError::Protocol("Session page omitted records"))?;
    let mut events = Vec::with_capacity(records.len());
    for record in records {
        let event = event_from_record(record)?;
        let seq = event_seq(event)?;
        if let Some((previous, _)) = events.last()
            && Some(*previous) != seq.checked_sub(1)
        {
            return Err(ClientError::Protocol(
                "Session page records are not consecutive",
            ));
        }
        events.push((seq, event.clone()));
    }

    Ok(ParsedPage {
        cursor,
        events,
        has_more,
    })
}

fn event_from_record(record: &Value) -> ClientResult<&Value> {
    if record.get("type").and_then(Value::as_str) != Some("event") {
        return Err(ClientError::Protocol(
            "Session page contained a non-event record",
        ));
    }
    record
        .get("event")
        .filter(|event| event.is_object())
        .ok_or(ClientError::Protocol(
            "Session page record omitted its event",
        ))
}

fn event_seq(event: &Value) -> ClientResult<i64> {
    event
        .get("seq")
        .and_then(Value::as_i64)
        .filter(|seq| *seq >= 0)
        .ok_or(ClientError::Protocol(
            "Session event omitted a valid sequence",
        ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn page(cursor: i64, seqs: &[i64], has_more: bool) -> Value {
        json!({
            "cursor": cursor,
            "hasMore": has_more,
            "records": seqs.iter().map(|seq| json!({
                "type": "event",
                "event": { "type": "session/title", "seq": seq },
            })).collect::<Vec<_>>(),
        })
    }

    fn snapshot(cursor: i64, seqs: &[i64], has_more: bool) -> Value {
        let mut value = page(cursor, seqs, has_more);
        value["type"] = json!("snapshot");
        value["header"] = json!({ "id": "session-1" });
        value
    }

    fn event(seq: i64) -> Value {
        json!({
            "type": "event",
            "event": { "type": "session/title", "seq": seq },
        })
    }

    fn seqs(events: &[Value]) -> Vec<i64> {
        events
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect()
    }

    #[test]
    fn snapshot_and_live_events_form_a_deduplicated_sequence() {
        let mut journal = SessionJournal::new("session-1");
        let update = journal.apply(&snapshot(1, &[0, 1], false)).unwrap();
        assert_eq!(seqs(&update.into_events()), [0, 1]);
        assert_eq!(journal.cursor(), Some(1));
        assert_eq!(
            journal.apply(&event(1)).unwrap().into_events(),
            Vec::<Value>::new()
        );
        assert_eq!(seqs(&journal.apply(&event(2)).unwrap().into_events()), [2]);
        assert_eq!(
            journal
                .apply(&json!({ "type": "assistant-stream", "frame": {} }))
                .unwrap()
                .into_events(),
            Vec::<Value>::new()
        );
        assert_eq!(journal.cursor(), Some(2));
    }

    #[test]
    fn paginated_reconnect_replaces_the_window_and_prepends_older_pages() {
        let mut journal = SessionJournal::new("session-1");
        journal.apply(&snapshot(1, &[0, 1], false)).unwrap();
        let update = journal.apply(&snapshot(4, &[3, 4], true)).unwrap();
        assert!(matches!(update, JournalUpdate::Replace(_)));
        assert_eq!(seqs(&update.into_events()), [3, 4]);
        assert_eq!(journal.window_start(), Some(3));
        assert!(journal.has_more());
        let older = journal.prepend_page(&page(4, &[0, 1, 2], false)).unwrap();
        assert_eq!(seqs(&older), [0, 1, 2]);
        assert_eq!(journal.window_start(), Some(0));
        assert!(!journal.has_more());
        assert_eq!(seqs(&journal.apply(&event(5)).unwrap().into_events()), [5]);
    }

    #[test]
    fn gaps_and_backwards_snapshots_do_not_change_the_cursor() {
        let mut journal = SessionJournal::new("session-1");
        journal.apply(&snapshot(1, &[0, 1], false)).unwrap();
        assert_eq!(
            journal.apply(&event(3)),
            Err(ClientError::Protocol(
                "Session event stream skipped a sequence"
            ))
        );
        assert_eq!(journal.cursor(), Some(1));
        assert_eq!(
            journal.apply(&snapshot(0, &[0], false)),
            Err(ClientError::Protocol(
                "Session snapshot moved the cursor back"
            ))
        );
        assert_eq!(journal.cursor(), Some(1));
    }

    #[test]
    fn malformed_windows_and_unmatched_pages_are_rejected() {
        let mut journal = SessionJournal::new("session-1");
        let mut wrong = snapshot(0, &[0], false);
        wrong["header"]["id"] = json!("session-2");
        assert_eq!(
            journal.apply(&wrong),
            Err(ClientError::Protocol("Session snapshot has the wrong id"))
        );
        assert_eq!(
            journal.apply(&snapshot(3, &[0, 2], false)),
            Err(ClientError::Protocol(
                "Session page records are not consecutive"
            ))
        );
        assert_eq!(
            journal.apply(&snapshot(3, &[2], false)),
            Err(ClientError::Protocol(
                "Session snapshot records do not meet its cursor"
            ))
        );
        assert_eq!(journal.cursor(), None);

        journal.apply(&snapshot(4, &[3, 4], true)).unwrap();
        assert_eq!(
            journal.prepend_page(&page(4, &[0, 1], true)),
            Err(ClientError::Protocol(
                "Session page does not meet the current window"
            ))
        );
        assert_eq!(journal.window_start(), Some(3));
        assert!(journal.has_more());
        assert_eq!(
            journal.prepend_page(&page(3, &[2], true)),
            Err(ClientError::Protocol(
                "Session page used a different history cutoff"
            ))
        );
    }

    #[test]
    fn empty_history_has_a_consistent_cursor_and_window() {
        let mut journal = SessionJournal::new("session-1");
        assert_eq!(
            journal.apply(&snapshot(-1, &[], false)).unwrap(),
            JournalUpdate::Replace(Vec::new())
        );
        assert_eq!(journal.cursor(), Some(-1));
        assert_eq!(journal.window_start(), None);
        assert!(!journal.has_more());
    }
}
