//! ntfy.sh push-notification dispatch.
//!
//! Optional, off by default — enable with `--features ntfy` at build time.
//! Pairs with the `[notifications.ntfy]` block in `config.toml`:
//!
//! ```toml
//! [notifications.ntfy]
//! enabled    = true
//! server     = "https://ntfy.sh"
//! topic      = "mrm-<your-secret-topic>"
//! priority   = "default"           # optional: min|low|default|high|max
//! auth_token = "tk_..."            # optional, self-hosted with auth only
//! ```
//!
//! Subscribe to the same topic in the [ntfy mobile app] and your phone gets
//! pushes whenever the daemon detects new chapters.
//!
//! [ntfy mobile app]: https://ntfy.sh/

use std::time::Duration;

use crate::config::NtfyConfig;

/// POST a notification to the configured ntfy topic.
///
/// Errors are logged but never propagated — a broken phone path must never
/// kill the polling loop or block desktop notifications.
pub async fn dispatch(cfg: &NtfyConfig, summary: &str, body: &str) {
    if !cfg.enabled {
        return;
    }
    if cfg.topic.trim().is_empty() {
        eprintln!("mrm: ntfy topic is empty — skipping remote notification");
        return;
    }

    let url = format!(
        "{}/{}",
        cfg.server.trim_end_matches('/'),
        cfg.topic.trim(),
    );

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mrm: ntfy client init failed: {e}");
            return;
        }
    };

    let mut req = client
        .post(&url)
        .body(body.to_string())
        .header("Title", summary)
        .header("Tags", "books");

    if let Some(p) = cfg.priority.as_deref() {
        req = req.header("Priority", p);
    }
    if let Some(t) = cfg.auth_token.as_deref() {
        req = req.header("Authorization", format!("Bearer {t}"));
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            eprintln!(
                "mrm: ntfy returned {} for {url}",
                resp.status(),
            );
        }
        Err(e) => eprintln!("mrm: ntfy dispatch failed: {e}"),
    }
}
