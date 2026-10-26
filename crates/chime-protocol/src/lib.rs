//! Shared wire types for Chime's notification send flow.

use serde::{Deserialize, Serialize};

/// A JSON notification request accepted by the send API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationRequest {
    pub title: String,
    pub body: Option<String>,
    pub image_url: Option<String>,
    pub url: Option<String>,
}

/// A versioned push payload handled by the browser service worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PushPayload {
    Notify {
        version: u8,
        id: String,
        title: String,
        body: Option<String>,
        image_url: Option<String>,
        url: Option<String>,
        expires_at: i64,
        ack_capability: String,
    },
    Dismiss {
        version: u8,
        id: String,
        expires_at: i64,
    },
}

/// The outcome of attempting to deliver one notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Accepted,
    Partial,
    Failed,
    NoDevices,
}

/// A summary of push-provider responses for a notification send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverySummary {
    pub id: String,
    pub attempted: u32,
    pub accepted: u32,
    pub failed: u32,
    pub expired_subscriptions_removed: u32,
    pub outcome: DeliveryOutcome,
}

#[cfg(test)]
mod tests {
    use super::{DeliveryOutcome, DeliverySummary, NotificationRequest, PushPayload};

    #[test]
    fn notification_request_uses_the_proposal_json_fields() {
        let request: NotificationRequest =
            serde_json::from_str(r#"{"title":"Build completed"}"#).unwrap();

        assert_eq!(
            request,
            NotificationRequest {
                title: "Build completed".into(),
                body: None,
                image_url: None,
                url: None,
            }
        );
    }

    #[test]
    fn notification_request_rejects_unknown_and_duplicate_fields() {
        assert!(
            serde_json::from_str::<NotificationRequest>(r#"{"title":"Hello","unexpected":true}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<NotificationRequest>(r#"{"title":"Hello","title":"Again"}"#)
                .is_err()
        );
    }

    #[test]
    fn delivery_summary_uses_snake_case_outcomes() {
        let summary = DeliverySummary {
            id: "n-1".into(),
            attempted: 1,
            accepted: 1,
            failed: 0,
            expired_subscriptions_removed: 0,
            outcome: DeliveryOutcome::Accepted,
        };

        let json = serde_json::to_value(summary).unwrap();
        assert_eq!(json["outcome"], "accepted");
        assert_eq!(json["expired_subscriptions_removed"], 0);
    }

    #[test]
    fn push_payloads_have_explicit_versioned_variants() {
        let notify = PushPayload::Notify {
            version: 1,
            id: "A".repeat(22),
            title: "Build complete".into(),
            body: None,
            image_url: None,
            url: None,
            expires_at: 1_800_000_000,
            ack_capability: "B".repeat(43),
        };
        let dismiss = PushPayload::Dismiss {
            version: 1,
            id: "A".repeat(22),
            expires_at: 1_800_000_000,
        };

        let notify = serde_json::to_value(notify).unwrap();
        let dismiss = serde_json::to_value(dismiss).unwrap();
        assert_eq!(notify["type"], "notify");
        assert_eq!(notify["version"], 1);
        assert_eq!(dismiss["type"], "dismiss");
        assert_eq!(dismiss["version"], 1);
        assert!(!dismiss.as_object().unwrap().contains_key("title"));
        assert!(serde_json::from_value::<PushPayload>(dismiss).is_ok());
        assert!(serde_json::from_str::<PushPayload>(r#"{"type":"unknown","version":1}"#).is_err());
    }
}
