use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::PublicKey;
use serde::Deserialize;
use url::Url;

pub const SUPPORTED_PUSH_HOSTS: [&str; 3] = [
    "fcm.googleapis.com",
    "updates.push.services.mozilla.com",
    "web.push.apple.com",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
    #[serde(default, rename = "expirationTime")]
    pub(crate) _expiration_time: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushSubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone)]
pub struct ValidatedSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub fn validate_subscription(
    subscription: PushSubscription,
) -> Result<ValidatedSubscription, &'static str> {
    let endpoint = Url::parse(&subscription.endpoint).map_err(|_| "invalid endpoint")?;
    if endpoint.scheme() != "https"
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port().is_some_and(|port| port != 443)
        || endpoint.path() == "/"
        || subscription.endpoint.len() > 2048
        || !endpoint
            .host_str()
            .is_some_and(|host| SUPPORTED_PUSH_HOSTS.contains(&host))
    {
        return Err("unsupported endpoint");
    }

    let p256dh = URL_SAFE_NO_PAD
        .decode(&subscription.keys.p256dh)
        .map_err(|_| "invalid subscription key")?;
    if p256dh.len() != 65
        || p256dh.first() != Some(&4)
        || PublicKey::from_sec1_bytes(&p256dh).is_err()
    {
        return Err("invalid subscription key");
    }

    let auth = URL_SAFE_NO_PAD
        .decode(&subscription.keys.auth)
        .map_err(|_| "invalid subscription key")?;
    if auth.len() != 16 {
        return Err("invalid subscription key");
    }

    Ok(ValidatedSubscription {
        endpoint: endpoint.to_string(),
        p256dh: subscription.keys.p256dh,
        auth: subscription.keys.auth,
    })
}

#[cfg(test)]
mod tests {
    use super::{PushSubscription, PushSubscriptionKeys, validate_subscription};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use p256::{SecretKey, elliptic_curve::Generate};

    fn valid_subscription(endpoint: &str) -> PushSubscription {
        let secret = SecretKey::try_generate().unwrap();
        PushSubscription {
            endpoint: endpoint.to_owned(),
            _expiration_time: None,
            keys: PushSubscriptionKeys {
                p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_bytes()),
                auth: URL_SAFE_NO_PAD.encode([7_u8; 16]),
            },
        }
    }

    #[test]
    fn accepts_only_supported_https_push_origins_and_valid_key_material() {
        for endpoint in [
            "https://fcm.googleapis.com/fcm/send/token",
            "https://updates.push.services.mozilla.com/wpush/v2/token",
            "https://web.push.apple.com/token",
        ] {
            assert!(validate_subscription(valid_subscription(endpoint)).is_ok());
        }
        for endpoint in [
            "http://fcm.googleapis.com/fcm/send/token",
            "https://fcm.googleapis.com.evil.example/token",
            "https://user@fcm.googleapis.com/token",
            "https://fcm.googleapis.com:444/token",
            "https://fcm.googleapis.com/token?target=https://example.com",
            "https://127.0.0.1/token",
            "https://fcm.googleapis.com/",
        ] {
            assert!(validate_subscription(valid_subscription(endpoint)).is_err());
        }
    }

    #[test]
    fn rejects_malformed_subscription_key_material() {
        let mut subscription = valid_subscription("https://fcm.googleapis.com/token");
        subscription.keys.p256dh = URL_SAFE_NO_PAD.encode([4_u8; 65]);
        assert!(validate_subscription(subscription).is_err());

        let mut subscription = valid_subscription("https://fcm.googleapis.com/token");
        subscription.keys.auth = URL_SAFE_NO_PAD.encode([1_u8; 15]);
        assert!(validate_subscription(subscription).is_err());

        let mut subscription = valid_subscription("https://fcm.googleapis.com/token");
        subscription.keys.auth.push('=');
        assert!(validate_subscription(subscription).is_err());
    }
}
