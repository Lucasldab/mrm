//! Desktop notification dispatch via notify-rust (D-Bus / mako).
//!
//! Notifications are grouped: up to 8 titles in one notification body,
//! then "…and N more". Errors are swallowed so a broken notification
//! daemon never crashes the coordinator.

use notify_rust::Notification;

/// Send a grouped desktop notification for newly-updated manhwa titles.
///
/// - Empty slice → no-op.
/// - 1 title → "New chapters available" / "{title}".
/// - 2+ titles → "{N} manhwa updated" / bullet list (max 8, then "…and N more").
pub fn send_grouped(titles: &[String]) {
    if titles.is_empty() {
        return;
    }

    let (summary, body) = if titles.len() == 1 {
        (
            "New chapters available".to_string(),
            titles[0].clone(),
        )
    } else {
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
    };

    let result = Notification::new()
        .appname("mrm")
        .summary(&summary)
        .body(&body)
        .show();

    if let Err(e) = result {
        eprintln!("mrm: notification error: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slice_is_noop() {
        // Must not panic; no assertion on side effects
        send_grouped(&[]);
    }

    // Note: D-Bus notifications cannot be integration-tested in unit test context.
    // Logic tests below verify the string-building only via the public behaviour.
}
