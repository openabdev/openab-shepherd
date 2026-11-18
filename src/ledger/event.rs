//! Events: the only thing the log stores, and the JSON they are written as.

use super::types::{
    BadContentRef, Code, ContentRef, DispatchId, NameError, Outcome, RejectReason, SessionId,
};
use serde_json::{json, Map, Value};

/// One fact appended to the log. Times are milliseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Created {
        id: DispatchId,
        content: ContentRef,
        owner: SessionId,
        created_by: Code,
        depends_on: Vec<DispatchId>,
        max_retries: u32,
        at: u64,
    },
    /// The intent to inject, logged before the injection is asked for.
    DeliveryStarted {
        id: DispatchId,
        attempt: u32,
        request_id: Code,
        at: u64,
    },
    /// The runtime confirmed the injection.
    Delivered {
        id: DispatchId,
        attempt: u32,
        request_id: Code,
        at: u64,
    },
    /// The runtime refused. Nothing was injected.
    DeliveryRefused {
        id: DispatchId,
        attempt: u32,
        request_id: Code,
        reason: Code,
        at: u64,
    },
    /// The answer is lost: a crash, or a timeout.
    DeliveryUnknown {
        id: DispatchId,
        attempt: u32,
        request_id: Code,
        reason: Code,
        at: u64,
    },
    /// The runtime's own audit shows the request was not injected.
    DeliveryNotInjected {
        id: DispatchId,
        attempt: u32,
        request_id: Code,
        at: u64,
    },
    /// The turn ended without a record and a retry is allowed.
    RetryRequested {
        id: DispatchId,
        attempt: u32,
        at: u64,
    },
    /// A worker record that passed the matching rule.
    RecordAccepted {
        id: DispatchId,
        attempt: u32,
        session: SessionId,
        outcome: Outcome,
        summary: Option<ContentRef>,
        at: u64,
    },
    /// A worker record that did not. The claimed fields are what the sender
    /// said, capped in length, and never an identifier the ledger trusts.
    RecordRejected {
        claimed_id: String,
        claimed_attempt: u64,
        claimed_session: String,
        reason: RejectReason,
        at: u64,
    },
    /// An operator moved a blocked dispatch back to `ready`.
    Unblocked { id: DispatchId, at: u64 },
    /// Given up.
    Failed {
        id: DispatchId,
        reason: Code,
        at: u64,
    },
}

impl Event {
    /// The wire name of the event type.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Created { .. } => "created",
            Event::DeliveryStarted { .. } => "delivery_started",
            Event::Delivered { .. } => "delivered",
            Event::DeliveryRefused { .. } => "delivery_refused",
            Event::DeliveryUnknown { .. } => "delivery_unknown",
            Event::DeliveryNotInjected { .. } => "delivery_not_injected",
            Event::RetryRequested { .. } => "retry_requested",
            Event::RecordAccepted { .. } => "record_accepted",
            Event::RecordRejected { .. } => "record_rejected",
            Event::Unblocked { .. } => "unblocked",
            Event::Failed { .. } => "failed",
        }
    }

    /// The JSON object of this event, without the `v` and `seq` envelope.
    pub(crate) fn to_object(&self) -> Map<String, Value> {
        let mut o = Map::new();
        o.insert("type".into(), json!(self.kind()));
        match self {
            Event::Created {
                id,
                content,
                owner,
                created_by,
                depends_on,
                max_retries,
                at,
            } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("content_sha256".into(), json!(content.sha256()));
                o.insert("content_len".into(), json!(content.len()));
                o.insert("owner".into(), json!(owner.as_str()));
                o.insert("created_by".into(), json!(created_by.as_str()));
                let deps: Vec<&str> = depends_on.iter().map(DispatchId::as_str).collect();
                o.insert("depends_on".into(), json!(deps));
                o.insert("max_retries".into(), json!(max_retries));
                o.insert("at".into(), json!(at));
            }
            Event::DeliveryStarted {
                id,
                attempt,
                request_id,
                at,
            } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("attempt".into(), json!(attempt));
                o.insert("request_id".into(), json!(request_id.as_str()));
                o.insert("at".into(), json!(at));
            }
            Event::RetryRequested { id, attempt, at } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("attempt".into(), json!(attempt));
                o.insert("at".into(), json!(at));
            }
            Event::Delivered {
                id,
                attempt,
                request_id,
                at,
            }
            | Event::DeliveryNotInjected {
                id,
                attempt,
                request_id,
                at,
            } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("attempt".into(), json!(attempt));
                o.insert("request_id".into(), json!(request_id.as_str()));
                o.insert("at".into(), json!(at));
            }
            Event::DeliveryRefused {
                id,
                attempt,
                request_id,
                reason,
                at,
            }
            | Event::DeliveryUnknown {
                id,
                attempt,
                request_id,
                reason,
                at,
            } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("attempt".into(), json!(attempt));
                o.insert("request_id".into(), json!(request_id.as_str()));
                o.insert("reason".into(), json!(reason.as_str()));
                o.insert("at".into(), json!(at));
            }
            Event::RecordAccepted {
                id,
                attempt,
                session,
                outcome,
                summary,
                at,
            } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("attempt".into(), json!(attempt));
                o.insert("session".into(), json!(session.as_str()));
                o.insert("outcome".into(), json!(outcome.as_str()));
                if let Some(s) = summary {
                    o.insert("summary_sha256".into(), json!(s.sha256()));
                    o.insert("summary_len".into(), json!(s.len()));
                }
                o.insert("at".into(), json!(at));
            }
            Event::RecordRejected {
                claimed_id,
                claimed_attempt,
                claimed_session,
                reason,
                at,
            } => {
                o.insert("claimed_id".into(), json!(claimed_id));
                o.insert("claimed_attempt".into(), json!(claimed_attempt));
                o.insert("claimed_session".into(), json!(claimed_session));
                o.insert("reason".into(), json!(reason.as_str()));
                o.insert("at".into(), json!(at));
            }
            Event::Unblocked { id, at } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("at".into(), json!(at));
            }
            Event::Failed { id, reason, at } => {
                o.insert("id".into(), json!(id.as_str()));
                o.insert("reason".into(), json!(reason.as_str()));
                o.insert("at".into(), json!(at));
            }
        }
        o
    }

    /// Parse an event from the JSON object of one log line.
    pub(crate) fn from_object(o: &Map<String, Value>) -> Result<Self, String> {
        let kind = str_field(o, "type")?;
        let at = u64_field(o, "at")?;
        Ok(match kind {
            "created" => Event::Created {
                id: id_field(o, "id")?,
                content: ContentRef::new(
                    str_field(o, "content_sha256")?,
                    u64_field(o, "content_len")?,
                )
                .map_err(|e: BadContentRef| format!("content_sha256: {e}"))?,
                owner: session_field(o, "owner")?,
                created_by: code_field(o, "created_by")?,
                depends_on: o
                    .get("depends_on")
                    .and_then(Value::as_array)
                    .ok_or("depends_on: missing or not an array")?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| "depends_on: not a string".to_string())
                            .and_then(|s| DispatchId::new(s).map_err(name_err("depends_on")))
                    })
                    .collect::<Result<_, _>>()?,
                max_retries: u32_field(o, "max_retries")?,
                at,
            },
            "delivery_started" => Event::DeliveryStarted {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                request_id: code_field(o, "request_id")?,
                at,
            },
            "delivered" => Event::Delivered {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                request_id: code_field(o, "request_id")?,
                at,
            },
            "delivery_refused" => Event::DeliveryRefused {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                request_id: code_field(o, "request_id")?,
                reason: code_field(o, "reason")?,
                at,
            },
            "delivery_unknown" => Event::DeliveryUnknown {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                request_id: code_field(o, "request_id")?,
                reason: code_field(o, "reason")?,
                at,
            },
            "delivery_not_injected" => Event::DeliveryNotInjected {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                request_id: code_field(o, "request_id")?,
                at,
            },
            "retry_requested" => Event::RetryRequested {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                at,
            },
            "record_accepted" => Event::RecordAccepted {
                id: id_field(o, "id")?,
                attempt: u32_field(o, "attempt")?,
                session: session_field(o, "session")?,
                outcome: Outcome::parse(str_field(o, "outcome")?)
                    .ok_or("outcome: unknown value")?,
                summary: match o.get("summary_sha256") {
                    None => None,
                    Some(_) => Some(
                        ContentRef::new(
                            str_field(o, "summary_sha256")?,
                            u64_field(o, "summary_len")?,
                        )
                        .map_err(|e: BadContentRef| format!("summary_sha256: {e}"))?,
                    ),
                },
                at,
            },
            "record_rejected" => Event::RecordRejected {
                claimed_id: str_field(o, "claimed_id")?.to_string(),
                claimed_attempt: u64_field(o, "claimed_attempt")?,
                claimed_session: str_field(o, "claimed_session")?.to_string(),
                reason: RejectReason::parse(str_field(o, "reason")?)
                    .ok_or("reason: unknown value")?,
                at,
            },
            "unblocked" => Event::Unblocked {
                id: id_field(o, "id")?,
                at,
            },
            "failed" => Event::Failed {
                id: id_field(o, "id")?,
                reason: code_field(o, "reason")?,
                at,
            },
            other => return Err(format!("unknown event type {other:?}")),
        })
    }
}

fn name_err(field: &'static str) -> impl Fn(NameError) -> String {
    move |e| format!("{field}: {e}")
}

fn str_field<'a>(o: &'a Map<String, Value>, k: &str) -> Result<&'a str, String> {
    o.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{k}: missing or not a string"))
}

fn u64_field(o: &Map<String, Value>, k: &str) -> Result<u64, String> {
    o.get(k)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{k}: missing or not an unsigned integer"))
}

fn u32_field(o: &Map<String, Value>, k: &str) -> Result<u32, String> {
    u32::try_from(u64_field(o, k)?).map_err(|_| format!("{k}: does not fit 32 bits"))
}

fn id_field(o: &Map<String, Value>, k: &'static str) -> Result<DispatchId, String> {
    DispatchId::new(str_field(o, k)?).map_err(name_err(k))
}

fn session_field(o: &Map<String, Value>, k: &'static str) -> Result<SessionId, String> {
    SessionId::new(str_field(o, k)?).map_err(name_err(k))
}

fn code_field(o: &Map<String, Value>, k: &'static str) -> Result<Code, String> {
    Code::new(str_field(o, k)?).map_err(name_err(k))
}
