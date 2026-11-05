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

/// Reduce a ntfy URL to `host[:port]` so logs never carry the topic (a bearer
/// secret) or credentials. Unparseable input yields a fixed placeholder.
pub(crate) fn redact_server(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => "<ntfy server>".to_string(),
        },
        Err(_) => "<ntfy server>".to_string(),
    }
}

/// Error line for a non-success HTTP status; names only the server host.
pub(crate) fn status_error(server: &str, status: reqwest::StatusCode) -> String {
    format!("mrm: ntfy returned {status} from {}", redact_server(server))
}

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
            eprintln!("{}", status_error(&cfg.server, resp.status()));
        }
        // reqwest errors embed the full URL (topic included); strip it.
        Err(e) => eprintln!("mrm: ntfy dispatch failed: {}", e.without_url()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_topic_and_path() {
        let h = redact_server("https://ntfy.example.org/mrm-secret-topic");
        assert_eq!(h, "ntfy.example.org");
        assert!(!h.contains("secret"));
    }

    #[test]
    fn redacts_server_with_port_and_credentials() {
        assert_eq!(redact_server("http://user:pw@10.0.0.1:8080/x"), "10.0.0.1:8080");
    }

    #[test]
    fn unparseable_url_gives_placeholder() {
        assert_eq!(redact_server("not a url topic-secret"), "<ntfy server>");
    }

    #[test]
    fn status_error_message_has_no_topic() {
        let m = status_error("https://ntfy.example.org", reqwest::StatusCode::FORBIDDEN);
        assert!(m.contains("403") && m.contains("ntfy.example.org"), "{m}");
    }
}
