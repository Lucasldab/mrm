//! Notification dispatch.
//!
//! Two backends:
//!   - Local desktop via `notify-rust` (D-Bus / mako). Always compiled.
//!   - Remote push via ntfy.sh, gated behind the `ntfy` Cargo feature.
//!
//! Notifications are grouped: up to 8 titles in one notification body,
//! then "…and N more". Errors are swallowed so a broken backend never
//! crashes the coordinator.

use notify_rust::Notification;

use crate::config::NotificationsConfig;

/// Send a grouped desktop notification for newly-updated manhwa titles.
///
/// - Empty slice → no-op.
/// - 1 title → "New chapters available" / "{title}".
/// - 2+ titles → "{N} manhwa updated" / bullet list (max 8, then "…and N more").
pub fn send_grouped(titles: &[String]) {
    if titles.is_empty() {
        return;
    }

    let (summary, body) = build_summary_body(titles);

    let result = Notification::new()
        .appname("mrm")
        .summary(&summary)
        .body(&body)
        .show();

    if let Err(e) = result {
        eprintln!("mrm: notification error: {e}");
    }
}

/// Send the same grouped notification to ntfy.sh / a self-hosted ntfy server.
///
/// No-op when the `ntfy` feature is disabled, when the title list is empty,
/// or when no `[notifications.ntfy]` block is configured. Errors are logged
/// to stderr but never propagated.
#[cfg(feature = "ntfy")]
pub async fn send_grouped_remote(notifications: &NotificationsConfig, titles: &[String]) {
    if titles.is_empty() {
        return;
    }
    let Some(ntfy_cfg) = notifications.ntfy.as_ref() else { return };

    let (summary, body) = build_summary_body(titles);
    crate::ntfy::dispatch(ntfy_cfg, &summary, &body).await;
}

#[cfg(not(feature = "ntfy"))]
pub async fn send_grouped_remote(_notifications: &NotificationsConfig, _titles: &[String]) {}

/// Build the (summary, body) pair shared by every notification backend.
pub(crate) fn build_summary_body(titles: &[String]) -> (String, String) {
    if titles.len() == 1 {
        return (
            "New chapters available".to_string(),
            titles[0].clone(),
        );
    }

    let shown = titles.len().min(8);
    let mut body = titles[..shown]
        .iter()
        .map(|t| format!("• {t}"))
        .collect::<Vec<_>>()
        .join("\n");
    if titles.len() > 8 {
        body.push_str(&format!("\n…and {} more", titles.len() - 8));
    }
    (format!("{} manhwa updated", titles.len()), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slice_is_noop() {
        // Must not panic; no assertion on side effects
        send_grouped(&[]);
    }

    #[test]
    fn summary_body_single_title() {
        let (s, b) = build_summary_body(&["Solo Leveling".to_string()]);
        assert_eq!(s, "New chapters available");
        assert_eq!(b, "Solo Leveling");
    }

    #[test]
    fn summary_body_multiple_titles() {
        let titles = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let (s, b) = build_summary_body(&titles);
        assert_eq!(s, "3 manhwa updated");
        assert_eq!(b, "• A\n• B\n• C");
    }

    #[test]
    fn summary_body_truncates_past_eight() {
        let titles: Vec<String> = (1..=10).map(|n| format!("T{n}")).collect();
        let (s, b) = build_summary_body(&titles);
        assert_eq!(s, "10 manhwa updated");
        assert!(b.ends_with("…and 2 more"));
        assert_eq!(b.matches('•').count(), 8);
    }

    // Note: D-Bus and HTTP cannot be integration-tested in unit test context.
}
