//! Live OAuth-based quota fetcher for Claude Code and Codex CLI.
//!
//! Bypasses tu's `live-frame-cache.json` (which only updates while `tu live`
//! is running) by calling the official Anthropic and ChatGPT usage endpoints
//! directly. Tokens are read from the credential files Claude Code and the
//! OpenAI CLI maintain locally.

use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CODEX_CLIENT_ID:  &str = "app_EMoamEEZ73f0CkXaXp7hranng";
const CLOCK_SKEW_SECS:  i64  = 60;

#[derive(Clone, Default, Debug)]
pub struct QuotaSnapshot {
    pub claude_primary_pct:   Option<f64>,
    pub claude_secondary_pct: Option<f64>,
    pub claude_resets_at:     Option<i64>,   // unix seconds
    pub codex_primary_pct:    Option<f64>,
    pub codex_secondary_pct:  Option<f64>,
}

/// Fetch quota for both providers. Best-effort: if one fails the other still
/// fills in. Result fields are None when unavailable.
pub fn fetch_quota_now() -> QuotaSnapshot {
    let mut snap = QuotaSnapshot::default();

    if let Some((tok, _refresh)) = claude_access_token() {
        if let Some((p, s, r)) = fetch_claude_usage(&tok) {
            snap.claude_primary_pct   = Some(p);
            snap.claude_secondary_pct = Some(s);
            snap.claude_resets_at     = r;
        }
    }
    if let Some((tok, account_id, _refresh)) = codex_access_token() {
        if let Some((p, s)) = fetch_codex_usage(&tok, &account_id) {
            snap.codex_primary_pct   = Some(p);
            snap.codex_secondary_pct = Some(s);
        }
    }
    snap
}

// ─── Claude ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeOauth {
    access_token: String,
    refresh_token: String,
    expires_at: Option<i64>,   // milliseconds since epoch
}
#[derive(Debug, Deserialize)]
struct ClaudeCreds {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOauth,
}

fn claude_credentials_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

/// Returns (access_token, refresh_token) — refreshing the access token if it
/// is expired or unknown. Refreshed token is NOT written back to disk; we just
/// use it for this call.
fn claude_access_token() -> Option<(String, String)> {
    let path = claude_credentials_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let creds: ClaudeCreds = serde_json::from_str(&raw).ok()?;

    let now = chrono::Utc::now().timestamp();
    let expired = creds.claude_ai_oauth.expires_at
        .map(|ms| now >= (ms / 1000) - CLOCK_SKEW_SECS)
        .unwrap_or(true);

    if !expired {
        return Some((creds.claude_ai_oauth.access_token, creds.claude_ai_oauth.refresh_token));
    }

    // Token expired — refresh.
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": creds.claude_ai_oauth.refresh_token,
        "client_id": CLAUDE_CLIENT_ID,
    });
    let resp = ureq::post("https://platform.claude.com/v1/oauth/token")
        .set("Content-Type", "application/json")
        .send_json(&body).ok()?;
    let v: Value = resp.into_json().ok()?;
    let new_access = v["access_token"].as_str()?.to_string();
    Some((new_access, creds.claude_ai_oauth.refresh_token))
}

fn fetch_claude_usage(access_token: &str) -> Option<(f64, f64, Option<i64>)> {
    let resp = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {}", access_token))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call().ok()?;
    let v: Value = resp.into_json().ok()?;
    let primary   = v["primary_used_percent"].as_f64()?;
    let secondary = v["secondary_used_percent"].as_f64().unwrap_or(0.0);
    let resets    = v["primary_resets_at"].as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp());
    Some((primary, secondary, resets))
}

// ─── Codex ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    refresh_token: String,
}
#[derive(Debug, Deserialize)]
struct CodexAuth {
    tokens: CodexTokens,
    account_id: String,
}

fn codex_auth_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(PathBuf::from(home).join(".codex").join("auth.json"))
}

/// Returns (access_token, account_id, refresh_token). Always refreshes — the
/// auth file doesn't store an expiresAt and refresh is cheap enough (~200ms).
fn codex_access_token() -> Option<(String, String, String)> {
    let path = codex_auth_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let auth: CodexAuth = serde_json::from_str(&raw).ok()?;

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": auth.tokens.refresh_token,
        "client_id": CODEX_CLIENT_ID,
    });
    let access = match ureq::post("https://auth.openai.com/oauth/token")
        .set("Content-Type", "application/json")
        .send_json(&body)
    {
        Ok(resp) => {
            let v: Value = resp.into_json().ok()?;
            v["access_token"].as_str()?.to_string()
        }
        Err(_) => auth.tokens.access_token.clone(),  // fall back to stored token
    };
    Some((access, auth.account_id, auth.tokens.refresh_token))
}

fn fetch_codex_usage(access_token: &str, account_id: &str) -> Option<(f64, f64)> {
    let resp = ureq::get("https://chatgpt.com/backend-api/api/codex/usage/wham/usage")
        .set("Authorization", &format!("Bearer {}", access_token))
        .set("ChatGPT-Account-Id", account_id)
        .call().ok()?;
    let v: Value = resp.into_json().ok()?;

    // Field names vary; try each known shape.
    let primary = v["usage_pct"].as_f64()
        .or_else(|| v["used_percent"].as_f64())
        .or_else(|| v["primary_used_percent"].as_f64())
        .or_else(|| v["rate_limits"]["primary"]["used_percent"].as_f64())
        .unwrap_or(0.0);
    let secondary = v["secondary_used_percent"].as_f64()
        .or_else(|| v["weekly_used_percent"].as_f64())
        .or_else(|| v["rate_limits"]["secondary"]["used_percent"].as_f64())
        .unwrap_or(0.0);
    Some((primary, secondary))
}
