use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Raw OAuth credential payload as stored in the macOS Keychain by Claude Code.
/// We deserialize permissively so the binary blob round-trips even if Anthropic
/// adds new fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthCreds {
    #[serde(rename = "claudeAiOauth")]
    pub oauth: OauthInner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OauthInner {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since epoch.
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl OauthCreds {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(Error::Json)
    }

    pub fn expires_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.oauth.expires_at)
    }

    pub fn expires_in(&self) -> Option<Duration> {
        self.expires_at().duration_since(SystemTime::now()).ok()
    }

    pub fn is_expired(&self, leeway: Duration) -> bool {
        match self.expires_at().duration_since(SystemTime::now()) {
            Ok(remaining) => remaining < leeway,
            Err(_) => true,
        }
    }

    pub fn email(&self) -> Option<&str> {
        self.oauth.email.as_deref()
    }

    pub fn plan(&self) -> Option<&str> {
        self.oauth.subscription_type.as_deref()
    }
}

/// Positive evidence that two credential blobs belong to *different* accounts: both carry
/// an email and the emails differ. Absent emails are deliberately NOT a mismatch.
///
/// `email` is optional in the OAuth blob, and `cs` treats email-less blobs as first-class.
/// For the rotated-token sync on switch/refresh, the only credential we may safely refuse
/// to copy is one we can prove is a *different* account — otherwise we skip preserving the
/// outgoing account's freshly rotated refresh token and log the user out on switch-back.
/// So we block only the (Some, Some)-and-differ case; (None, None), (Some, None), and
/// (None, Some) all fall through as "not provably different" and are allowed to sync.
pub fn is_cross_account(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x != y)
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileSummary {
    pub name: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// ISO-8601 string for human readability in JSON output.
    pub expires_at: Option<String>,
    pub expires_in_secs: Option<i64>,
    pub last_used: Option<String>,
    pub is_active: bool,
    pub is_default: bool,
}

impl ProfileSummary {
    pub fn from_creds(name: &str, creds: &OauthCreds) -> Self {
        let expires_at = chrono::DateTime::<chrono::Utc>::from(creds.expires_at()).to_rfc3339();
        let expires_in_secs = creds
            .expires_at()
            .duration_since(SystemTime::now())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(|e| -(e.duration().as_secs() as i64));
        Self {
            name: name.to_string(),
            email: creds.email().map(|s| s.to_string()),
            plan: creds.plan().map(|s| s.to_string()),
            expires_at: Some(expires_at),
            expires_in_secs: Some(expires_in_secs),
            last_used: None,
            is_active: false,
            is_default: false,
        }
    }

    pub fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            email: None,
            plan: None,
            expires_at: None,
            expires_in_secs: None,
            last_used: None,
            is_active: false,
            is_default: false,
        }
    }
}

pub fn human_expiry(secs: i64) -> String {
    if secs <= 0 {
        return format!("expired {} ago", human_duration((-secs) as u64));
    }
    format!("in {}", human_duration(secs as u64))
}

pub fn human_duration(mut secs: u64) -> String {
    let days = secs / 86_400;
    secs %= 86_400;
    let hours = secs / 3_600;
    secs %= 3_600;
    let mins = secs / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_FUTURE: &[u8] = br#"{
        "claudeAiOauth": {
            "accessToken": "tok-abc",
            "refreshToken": "ref-xyz",
            "expiresAt": 99999999999000,
            "scopes": ["user:profile"],
            "subscriptionType": "max",
            "email": "user@example.com"
        }
    }"#;

    const FIXTURE_MINIMAL: &[u8] = br#"{
        "claudeAiOauth": {
            "accessToken": "tok",
            "refreshToken": "ref",
            "expiresAt": 1
        }
    }"#;

    const FIXTURE_EXPIRED: &[u8] = br#"{
        "claudeAiOauth": {
            "accessToken": "tok",
            "refreshToken": "ref",
            "expiresAt": 100,
            "email": "old@example.com",
            "subscriptionType": "pro"
        }
    }"#;

    #[test]
    fn parses_full_blob() {
        let c = OauthCreds::parse(FIXTURE_FUTURE).unwrap();
        assert_eq!(c.email(), Some("user@example.com"));
        assert_eq!(c.plan(), Some("max"));
        assert!(!c.is_expired(Duration::from_secs(60)));
    }

    #[test]
    fn parses_minimal_blob() {
        let c = OauthCreds::parse(FIXTURE_MINIMAL).unwrap();
        assert!(c.email().is_none());
        assert!(c.plan().is_none());
    }

    #[test]
    fn detects_expired() {
        let c = OauthCreds::parse(FIXTURE_EXPIRED).unwrap();
        assert!(c.is_expired(Duration::from_secs(60)));
    }

    #[test]
    fn summary_marks_negative_expiry() {
        let c = OauthCreds::parse(FIXTURE_EXPIRED).unwrap();
        let s = ProfileSummary::from_creds("p", &c);
        assert_eq!(s.name, "p");
        assert!(s.expires_in_secs.unwrap() <= 0);
    }

    #[test]
    fn human_duration_buckets() {
        assert_eq!(human_duration(30), "30s");
        assert_eq!(human_duration(120), "2m");
        assert_eq!(human_duration(3_700), "1h1m");
        assert_eq!(human_duration(90_000), "1d1h");
    }

    #[test]
    fn human_expiry_signs() {
        assert!(human_expiry(60).starts_with("in "));
        assert!(human_expiry(-60).starts_with("expired "));
    }
}
