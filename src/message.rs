//! Builds the plain-text daily digest message for IM channels (Telegram /
//! Discord / any chat app). Read-only: it formats already-stored items into a
//! numbered list of Chinese titles, each with a deep-link into the published
//! site's per-item anchor.

use crate::model::NewsItem;
use anyhow::{bail, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};

/// Default number of items listed in a daily message; overridable via the
/// `digest --top <N>` flag.
pub const DEFAULT_TOP: usize = 10;

/// The day an item belongs to: its published date, falling back to first-seen.
/// Mirrors `render::build_days` so the message matches the site's grouping.
fn item_day(it: &NewsItem, first_seen: DateTime<Utc>) -> String {
    it.published
        .unwrap_or(first_seen)
        .format("%Y-%m-%d")
        .to_string()
}

/// The most recent day with publishable content (the day `index.html` shows),
/// or `None` when nothing is publishable. Only complete items count, mirroring
/// the rendered site — a day made up solely of incomplete legacy rows isn't
/// rendered, so it shouldn't be the digest's "latest day" either.
pub fn latest_day(all: &[(NewsItem, DateTime<Utc>)]) -> Option<String> {
    all.iter()
        .filter(|(it, _)| it.is_complete())
        .map(|(it, fs)| item_day(it, *fs))
        .max()
}

/// Build the plain-text digest message for `date` (YYYY-MM-DD). `base_url` is an
/// absolute site root with no trailing slash, e.g. `https://ainews.dob.cc`.
/// Errors when the day has no items so a cron job won't post an empty message.
pub fn build_message(
    all: &[(NewsItem, DateTime<Utc>)],
    date: &str,
    base_url: &str,
    top: usize,
) -> Result<String> {
    let weekday = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| weekday_zh(d.weekday()))
        .map_err(|_| anyhow::anyhow!("invalid date {date:?} (expected YYYY-MM-DD)"))?;

    // Keep this day's items, ranked like the site: importance desc, then time
    // desc. Only complete items, matching what `build_days` actually renders —
    // otherwise a deep-link's `#id` anchor wouldn't exist on the page.
    let mut day: Vec<(&NewsItem, DateTime<Utc>)> = all
        .iter()
        .filter(|(it, fs)| it.is_complete() && item_day(it, *fs) == date)
        .map(|(it, fs)| (it, it.published.unwrap_or(*fs)))
        .collect();
    if day.is_empty() {
        bail!("no items for {date}");
    }
    day.sort_by(|a, b| {
        b.0.importance
            .unwrap_or(0)
            .cmp(&a.0.importance.unwrap_or(0))
            .then(b.1.cmp(&a.1))
    });
    day.truncate(top);

    let day_url = format!("{base_url}/{}", crate::render::day_path(date));

    let mut out = String::new();
    out.push_str(&format!("📰 Coding Agent 日报 · {date} ({weekday})\n"));
    for (i, (it, _)) in day.iter().enumerate() {
        // day holds only is_complete() items, so title_zh is always present;
        // the unwrap_or is just a defensive default.
        let title = it.title_zh.as_deref().unwrap_or(&it.title);
        out.push_str(&format!("\n{}. {}\n   {day_url}#{}\n", i + 1, title, it.id));
    }
    out.push_str(&format!("\n完整摘要 → {day_url}\n"));
    Ok(out)
}

fn weekday_zh(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete item, as the digest requires. `title_zh: Some` is the norm;
    /// `None` makes the item incomplete (used to test exclusion).
    fn item(
        url: &str,
        title: &str,
        title_zh: Option<&str>,
        imp: i64,
        day: &str,
    ) -> (NewsItem, DateTime<Utc>) {
        let mut it = NewsItem::new("Src", title, url);
        it.title_zh = title_zh.map(String::from);
        it.summary = Some("导语".into());
        it.body_md = Some("正文".into());
        it.title_en = Some(title.into());
        it.summary_en = Some("Lede".into());
        it.body_md_en = Some("Body".into());
        it.importance = Some(imp);
        let when = DateTime::parse_from_rfc3339(&format!("{day}T08:00:00Z"))
            .unwrap()
            .with_timezone(&Utc);
        it.published = Some(when);
        (it, when)
    }

    #[test]
    fn formats_header_titles_links_and_footer() {
        let items = vec![
            item(
                "https://example.com/a",
                "A",
                Some("标题甲"),
                90,
                "2026-05-26",
            ),
            item(
                "https://example.com/b",
                "B",
                Some("标题乙"),
                50,
                "2026-05-26",
            ),
        ];
        let msg = build_message(&items, "2026-05-26", "https://ainews.dob.cc", DEFAULT_TOP).unwrap();
        assert!(msg.contains("📰 Coding Agent 日报 · 2026-05-26 (周二)"));
        // Higher importance ranks first.
        let a_pos = msg.find("标题甲").unwrap();
        let b_pos = msg.find("标题乙").unwrap();
        assert!(a_pos < b_pos);
        assert!(msg.contains("1. 标题甲"));
        let id = items[0].0.id.clone();
        assert!(msg.contains(&format!("https://ainews.dob.cc/feeds/2026/05/26.html#{id}")));
        assert!(msg.contains("完整摘要 → https://ainews.dob.cc/feeds/2026/05/26.html"));
    }

    #[test]
    fn truncates_to_top_n() {
        let mut items = Vec::new();
        for i in 0..15 {
            items.push(item(
                &format!("https://example.com/{i}"),
                &format!("T{i}"),
                Some(&format!("标题{i}")),
                i as i64,
                "2026-05-26",
            ));
        }
        // Default keeps the top 10.
        let msg = build_message(&items, "2026-05-26", "https://ainews.dob.cc", DEFAULT_TOP).unwrap();
        assert!(msg.contains("10. ") && !msg.contains("11. "));
        // A custom --top is honored.
        let msg = build_message(&items, "2026-05-26", "https://ainews.dob.cc", 5).unwrap();
        assert!(msg.contains("5. ") && !msg.contains("6. "));
    }

    #[test]
    fn invalid_date_errors() {
        let items = vec![item(
            "https://example.com/a",
            "A",
            Some("甲"),
            90,
            "2026-05-26",
        )];
        let err = build_message(&items, "not-a-date", "https://ainews.dob.cc", DEFAULT_TOP).unwrap_err();
        assert!(err.to_string().contains("invalid date"));
    }

    #[test]
    fn empty_day_errors() {
        let items = vec![item(
            "https://example.com/a",
            "A",
            Some("甲"),
            90,
            "2026-05-26",
        )];
        assert!(build_message(&items, "2026-05-25", "https://ainews.dob.cc", DEFAULT_TOP).is_err());
    }

    #[test]
    fn excludes_incomplete_items() {
        // An item missing its Chinese title is not complete, so it must not be
        // listed — it isn't on the rendered page, so a deep-link to it would
        // dangle. (Note `item(.., None, ..)` fills every field but title_zh.)
        let incomplete = item("https://example.com/x", "Untranslated", None, 99, "2026-05-26");
        let complete = item("https://example.com/a", "A", Some("标题甲"), 50, "2026-05-26");
        let items = vec![incomplete, complete];
        let msg = build_message(&items, "2026-05-26", "https://ainews.dob.cc", DEFAULT_TOP).unwrap();
        assert!(msg.contains("标题甲"));
        assert!(!msg.contains("Untranslated"));
        // Only the one complete item is listed (no rank 2).
        assert!(msg.contains("1. 标题甲") && !msg.contains("2. "));
    }

    #[test]
    fn day_of_only_incomplete_items_errors() {
        // If every item for the day is incomplete, the digest has nothing to
        // publish and errors rather than emit dangling links.
        let mut bare = NewsItem::new("Src", "Bare", "https://example.com/bare");
        bare.published = Some(
            DateTime::parse_from_rfc3339("2026-05-26T08:00:00Z").unwrap().with_timezone(&Utc),
        );
        let items = vec![(bare.clone(), bare.published.unwrap())];
        assert!(build_message(&items, "2026-05-26", "https://ainews.dob.cc", DEFAULT_TOP).is_err());
    }

    #[test]
    fn latest_day_picks_max_date() {
        let items = vec![
            item("https://example.com/a", "A", Some("甲"), 90, "2026-05-24"),
            item("https://example.com/b", "B", Some("乙"), 90, "2026-05-26"),
        ];
        assert_eq!(latest_day(&items).as_deref(), Some("2026-05-26"));
    }

    #[test]
    fn latest_day_ignores_incomplete_items() {
        // The newest day holds only an incomplete row; latest_day must skip it
        // and return the newest day that actually has publishable content.
        let incomplete = item("https://example.com/new", "New", None, 90, "2026-05-26");
        let complete = item("https://example.com/old", "Old", Some("旧"), 90, "2026-05-24");
        assert_eq!(latest_day(&[incomplete, complete]).as_deref(), Some("2026-05-24"));
    }

    #[test]
    fn latest_day_is_none_when_all_incomplete() {
        let bare = NewsItem::new("Src", "Bare", "https://example.com/bare");
        let when = Utc::now();
        assert_eq!(latest_day(&[(bare, when)]), None);
    }
}
