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
    use super::{DeliveryOutcome, DeliverySummary, NotificationRequest};

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
}
