//! `duoray://` links: a subscription to add in one click (from a Telegram bot
//! through the hub's /add page, or a website). DUORAY never adds it silently:
//! the "Добавить" dialog opens with the link filled in.
//!
//!   duoray://add/<subscription url>          (raw or percent-encoded)
//!   duoray://add?url=<percent-encoded url>   (also `import` for `add`)

use url::Url;

pub const SCHEME: &str = "duoray://";

/// The subscription URL in a `duoray://` link; `None` for anything else.
pub fn subscription(link: &str) -> Option<String> {
    let rest = link.trim().strip_prefix(SCHEME)?;
    let (action, rest) = rest.split_once(['/', '?']).unwrap_or((rest, ""));
    if !matches!(action, "add" | "import") {
        return None;
    }
    let raw = match rest.strip_prefix("url=") {
        Some(q) => q,
        None => rest.split_once("?url=").map_or(rest, |(_, q)| q),
    };
    let decoded = percent_decode(raw);
    let url = Url::parse(decoded.trim()).ok()?;
    matches!(url.scheme(), "https" | "http").then(|| url.to_string())
}

/// The first `duoray://` link among the command-line arguments.
pub fn from_args() -> Option<String> {
    std::env::args().skip(1).find(|a| a.starts_with(SCHEME))
}

fn percent_decode(s: &str) -> String {
    // Already a plain URL (raw form): leave its own escapes alone.
    if s.starts_with("http://") || s.starts_with("https://") {
        return s.to_string();
    }
    url::form_urlencoded::parse(format!("u={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::subscription;

    #[test]
    fn reads_the_subscription_url() {
        let sub = "https://sub.example.com/abc?x=1";
        assert_eq!(subscription("duoray://add/https://sub.example.com/abc?x=1").as_deref(), Some(sub));
        assert_eq!(subscription("duoray://add/https%3A%2F%2Fsub.example.com%2Fabc%3Fx%3D1").as_deref(), Some(sub));
        assert_eq!(subscription("duoray://import?url=https%3A%2F%2Fsub.example.com%2Fabc%3Fx%3D1").as_deref(), Some(sub));
        assert_eq!(subscription("duoray://add/?url=https%3A%2F%2Fsub.example.com%2Fabc%3Fx%3D1").as_deref(), Some(sub));
        // Only http(s) subscriptions, only known actions.
        assert_eq!(subscription("duoray://add/file:///etc/passwd"), None);
        assert_eq!(subscription("duoray://add/javascript:alert(1)"), None);
        assert_eq!(subscription("duoray://connect/https://x.example"), None);
        assert_eq!(subscription("https://x.example"), None);
    }
}
