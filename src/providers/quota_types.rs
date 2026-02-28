//! Shared types for provider quota and rate limit tracking.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Quota metadata extracted from provider HTTP response headers or error messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaMetadata {
    /// Remaining requests/tokens in the current rate limit window
    pub rate_limit_remaining: Option<u64>,
    /// When the current rate limit window resets
    pub rate_limit_reset_at: Option<DateTime<Utc>>,
    /// Seconds to wait before retrying (from `Retry-After` header)
    pub retry_after_seconds: Option<u64>,
    /// Total request/token quota in the current window
    pub rate_limit_total: Option<u64>,
}

/// Status of a provider's quota and health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaStatus {
    /// Provider is healthy and within quota
    Ok,
    /// Provider is being rate-limited (requests throttled)
    RateLimited,
    /// Provider circuit breaker is open (too many failures)
    CircuitOpen,
    /// Provider quota is fully exhausted
    QuotaExhausted,
}

/// Quota information for a single OAuth/API profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileQuotaInfo {
    /// Profile name (e.g., "default", "org-workspace")
    pub profile_name: String,
    /// Current quota status for this profile
    pub status: QuotaStatus,
    /// Remaining requests in the current window
    pub rate_limit_remaining: Option<u64>,
    /// When the rate limit window resets
    pub rate_limit_reset_at: Option<DateTime<Utc>>,
    /// Total quota limit for this profile
    pub rate_limit_total: Option<u64>,
}

/// Quota and health information for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderQuotaInfo {
    /// Provider name (e.g., "openai", "anthropic")
    pub provider: String,
    /// Current aggregate status
    pub status: QuotaStatus,
    /// Number of consecutive failures recorded
    pub failure_count: u32,
    /// Last error message (if any)
    pub last_error: Option<String>,
    /// Seconds to wait before retrying (from error or header)
    pub retry_after_seconds: Option<u64>,
    /// When the circuit breaker will reset (if open)
    pub circuit_resets_at: Option<DateTime<Utc>>,
    /// Per-profile quota information
    pub profiles: Vec<ProfileQuotaInfo>,
}

/// Overall quota summary for all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSummary {
    /// When this summary was generated
    pub timestamp: DateTime<Utc>,
    /// Per-provider quota information
    pub providers: Vec<ProviderQuotaInfo>,
}

impl QuotaSummary {
    /// Returns names of providers with `QuotaStatus::Ok`.
    pub fn available_providers(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|p| p.status == QuotaStatus::Ok)
            .map(|p| p.provider.clone())
            .collect()
    }

    /// Returns names of providers that are rate-limited.
    pub fn rate_limited_providers(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|p| p.status == QuotaStatus::RateLimited)
            .map(|p| p.provider.clone())
            .collect()
    }

    /// Returns names of providers with an open circuit breaker.
    pub fn circuit_open_providers(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|p| p.status == QuotaStatus::CircuitOpen)
            .map(|p| p.provider.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_summary_available_providers() {
        let summary = QuotaSummary {
            timestamp: Utc::now(),
            providers: vec![
                ProviderQuotaInfo {
                    provider: "openai".to_string(),
                    status: QuotaStatus::Ok,
                    failure_count: 0,
                    last_error: None,
                    retry_after_seconds: None,
                    circuit_resets_at: None,
                    profiles: vec![],
                },
                ProviderQuotaInfo {
                    provider: "anthropic".to_string(),
                    status: QuotaStatus::RateLimited,
                    failure_count: 1,
                    last_error: None,
                    retry_after_seconds: Some(30),
                    circuit_resets_at: None,
                    profiles: vec![],
                },
            ],
        };
        assert_eq!(summary.available_providers(), vec!["openai"]);
        assert_eq!(summary.rate_limited_providers(), vec!["anthropic"]);
        assert!(summary.circuit_open_providers().is_empty());
    }

    #[test]
    fn quota_metadata_default_is_all_none() {
        let meta = QuotaMetadata::default();
        assert!(meta.rate_limit_remaining.is_none());
        assert!(meta.retry_after_seconds.is_none());
    }
}
