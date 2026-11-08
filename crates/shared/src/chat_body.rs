use std::fmt::Write;

/// Chat-webhook text: the status line, then the explanation as its own paragraph.
///
/// The explanation is model output; with `escape`, `& < >` become entities so Slack-style mrkdwn
/// can't turn it into mentions or links. Destinations that don't decode entities get it raw.
#[must_use]
pub fn status_text(
    monitor_name: &str,
    new_state: &str,
    explanation: Option<&str>,
    escape: bool,
) -> String {
    let mut t = format!("Monitor '{monitor_name}' is {new_state}");
    if let Some(e) = explanation {
        t.push_str("\n\n");
        if escape {
            t.push_str(&esc_entities(e));
        } else {
            t.push_str(e);
        }
    }
    t
}

/// Backslash-escapes the markdown metacharacters Teams and Discord render, so model output
/// can't format text or inject links.
#[must_use]
pub fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\*_`[]()#>~|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

const DISCORD_CONTENT_MAX_CHARS: usize = 2000;

/// JSON body for a text-webhook destination.
///
/// Discord gets its native `content` field (capped at 2000 chars) with `allowed_mentions` cleared
/// so `@everyone`/`@here` in model output can't ping; a `/slack`-suffixed Discord URL expects
/// Slack's `text` field instead.
#[must_use]
pub fn chat_body(
    dest_type: &str,
    monitor_name: &str,
    new_state: &str,
    explanation: Option<&str>,
    url: &str,
) -> serde_json::Value {
    let escape = matches!(dest_type, "slack" | "mattermost" | "google_chat");
    let md_escaped = matches!(dest_type, "microsoft_teams_webhook" | "discord")
        .then(|| explanation.map(escape_markdown))
        .flatten();
    let explanation = md_escaped.as_deref().or(explanation);
    let text = status_text(monitor_name, new_state, explanation, escape);
    chat_body_with_text(dest_type, text, url)
}

/// Wraps already-built message text in the destination type's webhook shape.
#[must_use]
pub fn chat_body_with_text(dest_type: &str, mut text: String, url: &str) -> serde_json::Value {
    if dest_type == "mattermost" {
        // Mattermost fires @channel/@all/@here from plain text; a zero-width space defuses them.
        text = defuse_at(&text);
    }
    if dest_type != "discord" {
        return serde_json::json!({ "text": text });
    }
    let field = if is_discord_slack_compat(url) {
        "text"
    } else {
        text = text.chars().take(DISCORD_CONTENT_MAX_CHARS).collect();
        "content"
    };
    serde_json::json!({ field: text, "allowed_mentions": { "parse": [] } })
}

const BRAND_ICON_URL: &str = "https://www.arcodash.com/icons/icon-256.png";
const AI_MARKER: &str = "\n\nAI analysis (confidence: ";
const TEST_TITLE: &str = "Test message from arcodash";
const TEST_LEAD: &str = "Alerts for your monitors will arrive here, formatted like this.";

/// Human label for a raw monitor state; the raw string never reaches chat copy.
#[must_use]
pub fn state_label(state: &str) -> &'static str {
    match state {
        "triggered" => "Alerting",
        "ok" => "OK",
        "unevaluable" => "Can't evaluate",
        _ => "Not checked yet",
    }
}

/// Short notification text (push preview, Slack `fallback`).
#[must_use]
pub fn default_subject(state: &str, name: &str) -> String {
    match state {
        "triggered" => format!("Alerting: {name}"),
        "ok" => format!("Recovered: {name}"),
        "unevaluable" => format!("Can't evaluate: {name}"),
        _ => format!("Not checked yet: {name}"),
    }
}

/// Message title.
#[must_use]
pub fn default_heading(state: &str, name: &str) -> String {
    match state {
        "triggered" => format!("{name} is alerting"),
        "ok" => format!("{name} is back to OK"),
        "unevaluable" => format!("{name} couldn't be evaluated"),
        _ => format!("{name} is not checked yet"),
    }
}

/// One-paragraph lead under the title.
#[must_use]
pub fn default_intro(state: &str) -> &'static str {
    match state {
        "triggered" => "The condition on this monitor is met.",
        "ok" => "The condition on this monitor is no longer met.",
        "unevaluable" => "The query ran, but the condition couldn't be checked against the result.",
        _ => "The monitor's state changed.",
    }
}

/// Everything a chat destination needs to render one monitor alert. `custom_subject` and
/// `custom_body` are already template-rendered; they replace the title/fallback and the lead only.
pub struct ChatAlert {
    pub monitor_name: String,
    pub monitor_url: Option<String>,
    pub state: String,
    pub observed: Option<String>,
    pub condition: String,
    pub reason: Option<String>,
    pub explanation: Option<String>,
    pub custom_subject: Option<String>,
    pub custom_body: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Splits an explanation into our stats line and the optional `(confidence, summary)` AI part.
#[must_use]
pub fn split_explanation(e: &str) -> (&str, Option<(&str, &str)>) {
    if let Some((stats, rest)) = e.split_once(AI_MARKER) {
        if let Some((conf, summary)) = rest.split_once("): ") {
            return (stats, Some((conf, summary)));
        }
    }
    (e, None)
}

struct Card {
    title: String,
    fallback: String,
    state: String,
    url: Option<String>,
    lead: String,
    what_changed: Option<String>,
    ai: Option<(String, String)>,
    facts: Vec<(&'static str, String)>,
    ts: i64,
}

impl Card {
    fn from_alert(a: &ChatAlert) -> Self {
        let name = &a.monitor_name;
        let (what_changed, ai) = a
            .explanation
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .map_or((None, None), |e| {
                let (stats, ai) = split_explanation(e);
                (
                    Some(stats.to_string()).filter(|s| !s.trim().is_empty()),
                    ai.map(|(c, s)| (c.to_string(), s.to_string())),
                )
            });
        let mut facts = Vec::new();
        if a.state == "unevaluable" {
            facts.push((
                "Reason",
                a.reason
                    .clone()
                    .unwrap_or_else(|| "No reason recorded.".into()),
            ));
        } else if let Some(o) = &a.observed {
            facts.push(("Observed", o.clone()));
        }
        facts.push(("Condition", a.condition.clone()));
        let subject = a.custom_subject.clone().filter(|t| !t.trim().is_empty());
        let body = a.custom_body.clone().filter(|t| !t.trim().is_empty());
        Self {
            title: subject
                .clone()
                .unwrap_or_else(|| default_heading(&a.state, name)),
            fallback: subject.unwrap_or_else(|| default_subject(&a.state, name)),
            state: a.state.clone(),
            url: a.monitor_url.clone(),
            lead: body.unwrap_or_else(|| default_intro(&a.state).to_string()),
            what_changed,
            ai,
            facts,
            ts: a.timestamp.timestamp(),
        }
    }

    fn test(ts: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            title: TEST_TITLE.into(),
            fallback: TEST_TITLE.into(),
            state: "test".into(),
            url: None,
            lead: TEST_LEAD.into(),
            what_changed: None,
            ai: None,
            facts: Vec::new(),
            ts: ts.timestamp(),
        }
    }

    fn ai_label(&self, esc: Esc) -> Option<String> {
        self.ai
            .as_ref()
            .map(|(c, _)| format!("AI analysis · {} confidence", esc(c)))
    }

    /// Lead then stats paragraph, escaped.
    fn body(&self, esc: Esc) -> String {
        let mut s = esc(&self.lead);
        if let Some(w) = &self.what_changed {
            s.push_str("\n\n");
            s.push_str(&esc(w));
        }
        s
    }

    /// `(name, value, short)` rows, escaped, empty values dropped.
    fn fields(&self, esc: Esc, value_cap: usize) -> Vec<(String, String, bool)> {
        let mut out: Vec<_> = self
            .facts
            .iter()
            .map(|(k, v)| (k.to_string(), trunc(&esc(v), value_cap), true))
            .collect();
        if let (Some(label), Some((_, s))) = (self.ai_label(esc), &self.ai) {
            out.push((trunc(&label, 256), trunc(&esc(s), value_cap), false));
        }
        out.retain(|(_, v, _)| !v.trim().is_empty());
        out
    }

    /// `(hex, discord int, Teams container style)`.
    fn colour(&self) -> (&'static str, u32, &'static str) {
        match self.state.as_str() {
            "triggered" => ("#ef4444", 15_680_580, "attention"),
            "ok" => ("#22c55e", 2_278_750, "good"),
            "unevaluable" => ("#f59e0b", 16_096_779, "warning"),
            _ => ("#737373", 7_566_195, "emphasis"),
        }
    }

    fn iso_ts(&self) -> String {
        chrono::DateTime::from_timestamp(self.ts, 0)
            .unwrap_or_default()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }
}

type Esc = fn(&str) -> String;

fn esc_entities(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn defuse_at(s: &str) -> String {
    s.replace('@', "@\u{200B}")
}

fn esc_mattermost(s: &str) -> String {
    defuse_at(&escape_markdown(&esc_entities(s)))
}

/// `<` becomes fullwidth so Slack link syntax (`<url|text>`) can't survive Discord's converter.
fn esc_discord_slack(s: &str) -> String {
    defuse_at(&escape_markdown(s)).replace('<', "\u{FF1C}")
}

/// Cuts to `cap` chars with a trailing `…`, never leaving a dangling escape backslash.
fn trunc(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut t: String = s.chars().take(cap - 1).collect();
    if t.ends_with('\\') {
        t.pop();
    }
    t.push('…');
    t
}

fn is_discord_slack_compat(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| u.path().trim_end_matches('/').ends_with("/slack"))
}

fn discord_embed_body(c: &Card) -> serde_json::Value {
    let esc: Esc = escape_markdown;
    let mut embed = serde_json::json!({
        "title": trunc(&esc(&c.title), 256),
        "color": c.colour().1,
        "description": trunc(&c.body(esc), 2000),
        "fields": c.fields(esc, 1024).into_iter()
            .map(|(n, v, short)| serde_json::json!({ "name": n, "value": v, "inline": short }))
            .collect::<Vec<_>>(),
        "footer": { "text": "arcodash" },
        "timestamp": c.iso_ts(),
    });
    if let Some(u) = &c.url {
        embed["url"] = u.clone().into();
    }
    serde_json::json!({
        "username": "arcodash",
        "avatar_url": BRAND_ICON_URL,
        "allowed_mentions": { "parse": [] },
        "embeds": [embed],
    })
}

fn attachment_body(c: &Card, esc: Esc) -> serde_json::Value {
    let mut a = serde_json::json!({
        "fallback": trunc(&esc(&c.fallback), 256),
        "color": c.colour().0,
        "title": trunc(&esc(&c.title), 256),
        "text": trunc(&c.body(esc), 3000),
        "fields": c.fields(esc, 1024).into_iter()
            .map(|(n, v, short)| serde_json::json!({ "title": n, "value": v, "short": short }))
            .collect::<Vec<_>>(),
        "footer": "arcodash",
        "footer_icon": BRAND_ICON_URL,
        "ts": c.ts,
    });
    if let Some(u) = &c.url {
        a["title_link"] = u.clone().into();
    }
    serde_json::json!({ "attachments": [a] })
}

fn teams_body(c: &Card) -> serde_json::Value {
    let esc: Esc = escape_markdown;
    let mut body = vec![
        serde_json::json!({ "type": "Container", "style": c.colour().2, "bleed": true, "items": [
            { "type": "TextBlock", "text": trunc(&esc(&c.title), 4000), "weight": "Bolder", "size": "Medium", "wrap": true }
        ]}),
        serde_json::json!({ "type": "TextBlock", "text": trunc(&c.body(esc), 4000), "wrap": true }),
    ];
    let facts: Vec<_> = c
        .facts
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| serde_json::json!({ "title": k, "value": trunc(&esc(v), 4000) }))
        .collect();
    if !facts.is_empty() {
        body.push(serde_json::json!({ "type": "FactSet", "facts": facts }));
    }
    if let (Some(label), Some((_, s))) = (c.ai_label(esc), &c.ai) {
        body.push(serde_json::json!({ "type": "Container", "separator": true, "spacing": "Medium", "items": [
            { "type": "TextBlock", "text": label, "weight": "Bolder", "size": "Small", "isSubtle": true },
            { "type": "TextBlock", "text": trunc(&esc(s), 4000), "wrap": true, "isSubtle": true }
        ]}));
    }
    let mut card = serde_json::json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "msteams": { "width": "Full" },
        "body": body,
    });
    if let Some(u) = &c.url {
        card["actions"] =
            serde_json::json!([{ "type": "Action.OpenUrl", "title": "View monitor", "url": u }]);
    }
    serde_json::json!({
        "type": "message",
        "attachments": [{ "contentType": "application/vnd.microsoft.card.adaptive", "content": card }],
    })
}

fn google_chat_body(c: &Card) -> serde_json::Value {
    const CAP: usize = 4000;
    let esc: Esc = esc_entities;
    let mut head = format!("*{}*", esc(&c.title));
    if let Some(u) = &c.url {
        let _ = write!(head, "\n<{u}|View monitor>");
    }
    let mut tail = String::new();
    let facts: Vec<String> = c
        .facts
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| format!("*{k}:* {}", esc(v)))
        .collect();
    if !facts.is_empty() {
        tail.push_str("\n\n");
        tail.push_str(&facts.join("\n"));
    }
    if let (Some(label), Some((_, s))) = (c.ai_label(esc), &c.ai) {
        let _ = write!(tail, "\n\n_{label}_\n{}", esc(s));
    }
    let budget = CAP.saturating_sub(head.chars().count() + tail.chars().count() + 2);
    let text = if budget == 0 {
        format!("{head}{tail}")
    } else {
        format!("{head}\n\n{}{tail}", trunc(&c.body(esc), budget))
    };
    serde_json::json!({ "text": trunc(&text, CAP) })
}

fn render_card(dest_type: &str, url: &str, c: &Card) -> serde_json::Value {
    match dest_type {
        "slack" => attachment_body(c, esc_entities),
        "mattermost" => attachment_body(c, esc_mattermost),
        "discord" if is_discord_slack_compat(url) => {
            let mut b = attachment_body(c, esc_discord_slack);
            b["allowed_mentions"] = serde_json::json!({ "parse": [] });
            b
        }
        "discord" => discord_embed_body(c),
        "microsoft_teams_webhook" => teams_body(c),
        "google_chat" => google_chat_body(c),
        _ => {
            serde_json::json!({ "text": format!("{}\n\n{}", c.title, c.body(std::string::ToString::to_string)) })
        }
    }
}

/// Destination-shaped message for a monitor alert. Generic webhooks keep using `chat_body`.
#[must_use]
pub fn chat_alert_body(dest_type: &str, url: &str, alert: &ChatAlert) -> serde_json::Value {
    render_card(dest_type, url, &Card::from_alert(alert))
}

/// Destination-shaped "Send test" message (same look as a real alert, grey, no facts).
#[must_use]
pub fn chat_test_body(
    dest_type: &str,
    url: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    render_card(dest_type, url, &Card::test(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_appends_explanation_paragraph() {
        assert_eq!(
            status_text("CPU", "triggered", None, true),
            "Monitor 'CPU' is triggered"
        );
        assert_eq!(
            status_text("CPU", "triggered", Some("Up 40% vs median"), true),
            "Monitor 'CPU' is triggered\n\nUp 40% vs median"
        );
    }

    const U: &str = "https://example.com/hook";

    #[test]
    fn slack_family_escapes_mrkdwn_control_chars() {
        for t in ["slack", "mattermost", "google_chat"] {
            assert_eq!(
                chat_body(
                    t,
                    "CPU",
                    "triggered",
                    Some("<!channel> a & <http://x|y>"),
                    U
                )["text"],
                "Monitor 'CPU' is triggered\n\n&lt;!channel&gt; a &amp; &lt;http://x|y&gt;"
            );
        }
    }

    #[test]
    fn mattermost_defuses_at_mentions_slack_unchanged() {
        let m = chat_body("mattermost", "CPU", "triggered", Some("@channel hi"), U);
        assert!(m["text"].as_str().unwrap().contains("@\u{200B}channel"));
        let s = chat_body("slack", "CPU", "triggered", Some("@channel hi"), U);
        assert!(s["text"].as_str().unwrap().contains("@channel"));
    }

    #[test]
    fn discord_native_uses_content_and_blocks_mentions() {
        let b = chat_body("discord", "CPU", "triggered", Some("a < b @everyone"), U);
        assert_eq!(
            b["content"],
            "Monitor 'CPU' is triggered\n\na < b @everyone"
        );
        assert!(b.get("text").is_none());
        assert_eq!(b["allowed_mentions"], serde_json::json!({ "parse": [] }));
    }

    #[test]
    fn discord_slack_suffix_keeps_text_shape() {
        for u in [
            "https://discord.com/api/webhooks/1/tok/slack",
            "https://discord.com/api/webhooks/1/tok/slack/?wait=true",
        ] {
            let b = chat_body("discord", "CPU", "triggered", None, u);
            assert_eq!(b["text"], "Monitor 'CPU' is triggered");
            assert!(b.get("content").is_none());
            assert_eq!(b["allowed_mentions"], serde_json::json!({ "parse": [] }));
        }
    }

    #[test]
    fn discord_content_truncated_on_char_boundary() {
        let long = "é".repeat(3000);
        let b = chat_body("discord", "CPU", "triggered", Some(&long), U);
        let c = b["content"].as_str().unwrap();
        assert_eq!(c.chars().count(), 2000);
        assert!(c.starts_with("Monitor 'CPU' is triggered"));
    }

    #[test]
    fn teams_and_discord_escape_markdown_in_explanation_only() {
        for t in ["microsoft_teams_webhook", "discord"] {
            let b = chat_body(
                t,
                "CPU",
                "triggered",
                Some("*b* _i_ `c` [x](u) #h > ~s~ a|b \\"),
                U,
            );
            let s = b["text"].as_str().or(b["content"].as_str()).unwrap();
            assert_eq!(
                s,
                "Monitor 'CPU' is triggered\n\n\\*b\\* \\_i\\_ \\`c\\` \\[x\\]\\(u\\) \\#h \\> \\~s\\~ a\\|b \\\\"
            );
        }
    }

    #[test]
    fn generic_webhook_leaves_text_raw() {
        let b = chat_body("webhook", "CPU", "triggered", Some("a < b & *c*"), U);
        assert_eq!(b["text"], "Monitor 'CPU' is triggered\n\na < b & *c*");
        assert!(b.get("allowed_mentions").is_none());
    }

    fn alert(state: &str) -> ChatAlert {
        ChatAlert {
            monitor_name: "Live check".into(),
            monitor_url: Some("https://app.example.com/monitors/42".into()),
            state: state.into(),
            observed: Some("12%".into()),
            condition: "Value is greater than 10".into(),
            reason: Some("Column missing".into()),
            explanation: Some(
                "Jumped to 12.\n\nAI analysis (confidence: high): Orders dropped.".into(),
            ),
            custom_subject: None,
            custom_body: None,
            timestamp: chrono::DateTime::from_timestamp(1791283680, 0).unwrap(),
        }
    }

    const DISCORD_SLACK: &str = "https://discord.com/api/webhooks/1/tok/slack";

    fn s(v: &serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn split_explanation_cases() {
        assert_eq!(split_explanation("stats"), ("stats", None));
        assert_eq!(
            split_explanation("a\n\nAI analysis (confidence: low): why"),
            ("a", Some(("low", "why")))
        );
        let bad = "a\n\nAI analysis (confidence: low";
        assert_eq!(split_explanation(bad), (bad, None));
    }

    #[test]
    fn discord_embed_per_state() {
        for (st, color, title) in [
            ("triggered", 15680580, "Live check is alerting"),
            ("ok", 2278750, "Live check is back to OK"),
            ("unevaluable", 16096779, "Live check couldn't be evaluated"),
        ] {
            let b = chat_alert_body("discord", U, &alert(st));
            let e = &b["embeds"][0];
            assert_eq!(e["title"], title);
            assert_eq!(e["color"], color);
            assert_eq!(e["url"], "https://app.example.com/monitors/42");
            assert_eq!(e["timestamp"], "2026-10-06T10:48:00Z");
            assert_eq!(e["footer"]["text"], "arcodash");
            assert_eq!(b["username"], "arcodash");
            assert_eq!(b["avatar_url"], BRAND_ICON_URL);
            assert_eq!(b["allowed_mentions"], serde_json::json!({ "parse": [] }));
            assert!(b.get("content").is_none());
            let f = e["fields"].as_array().unwrap();
            assert_eq!(f.len(), 3);
            assert_eq!(f[2]["name"], "AI analysis · high confidence");
            assert_eq!(f[2]["value"], "Orders dropped.");
            assert_eq!(f[2]["inline"], false);
            let first = if st == "unevaluable" {
                "Reason"
            } else {
                "Observed"
            };
            assert_eq!(f[0]["name"], first);
        }
        let b = chat_alert_body("discord", U, &alert("triggered"));
        assert_eq!(
            b["embeds"][0]["description"],
            "The condition on this monitor is met.\n\nJumped to 12."
        );
        assert_eq!(b["embeds"][0]["fields"][0]["value"], "12%");
    }

    #[test]
    fn unevaluable_reason_fallback() {
        let mut a = alert("unevaluable");
        a.reason = None;
        let b = chat_alert_body("discord", U, &a);
        assert_eq!(b["embeds"][0]["fields"][0]["value"], "No reason recorded.");
    }

    #[test]
    fn attachment_shape_slack_mattermost_discord_slack() {
        for (t, u) in [("slack", U), ("mattermost", U), ("discord", DISCORD_SLACK)] {
            for (st, color) in [
                ("triggered", "#ef4444"),
                ("ok", "#22c55e"),
                ("unevaluable", "#f59e0b"),
            ] {
                let b = chat_alert_body(t, u, &alert(st));
                let a = &b["attachments"][0];
                assert_eq!(a["color"], color);
                assert_eq!(a["title_link"], "https://app.example.com/monitors/42");
                assert_eq!(a["footer"], "arcodash");
                assert_eq!(a["footer_icon"], BRAND_ICON_URL);
                assert_eq!(a["ts"], 1791283680);
                assert!(b.get("text").is_none());
                assert_eq!(a["fields"].as_array().unwrap().len(), 3);
                assert_eq!(a["fields"][2]["title"], "AI analysis · high confidence");
            }
            let b = chat_alert_body(t, u, &alert("triggered"));
            assert_eq!(b["attachments"][0]["fallback"], "Alerting: Live check");
            assert_eq!(b["attachments"][0]["title"], "Live check is alerting");
            assert_eq!(b.get("allowed_mentions").is_some(), t == "discord", "{t}");
        }
    }

    #[test]
    fn teams_card_shape_for_alert_and_test() {
        let now = chrono::Utc::now();
        for b in [
            chat_alert_body("microsoft_teams_webhook", U, &alert("triggered")),
            chat_test_body("microsoft_teams_webhook", U, now),
        ] {
            assert_eq!(b["type"], "message");
            let att = &b["attachments"][0];
            assert_eq!(
                att["contentType"],
                "application/vnd.microsoft.card.adaptive"
            );
            let c = &att["content"];
            assert_eq!(c["type"], "AdaptiveCard");
            assert_eq!(c["version"], "1.4");
            assert!(c["body"].as_array().is_some_and(|a| !a.is_empty()));
        }
        let c = &chat_alert_body("microsoft_teams_webhook", U, &alert("ok"))["attachments"][0]
            ["content"];
        assert_eq!(c["body"][0]["style"], "good");
        assert_eq!(c["body"][2]["type"], "FactSet");
        assert_eq!(
            c["body"][3]["items"][0]["text"],
            "AI analysis · high confidence"
        );
        assert_eq!(
            c["actions"][0]["url"],
            "https://app.example.com/monitors/42"
        );
    }

    #[test]
    fn teams_omits_empty_parts() {
        let mut a = alert("triggered");
        a.explanation = None;
        a.observed = None;
        a.condition = String::new();
        a.monitor_url = None;
        let c = &chat_alert_body("microsoft_teams_webhook", U, &a)["attachments"][0]["content"];
        assert_eq!(c["body"].as_array().unwrap().len(), 2);
        assert!(c.get("actions").is_none());
    }

    #[test]
    fn google_chat_text_layout() {
        let b = chat_alert_body("google_chat", U, &alert("triggered"));
        assert_eq!(
            b["text"],
            "*Live check is alerting*\n<https://app.example.com/monitors/42|View monitor>\n\nThe condition on this monitor is met.\n\nJumped to 12.\n\n*Observed:* 12%\n*Condition:* Value is greater than 10\n\n_AI analysis · high confidence_\nOrders dropped."
        );
    }

    #[test]
    fn test_message_per_destination() {
        let now = chrono::DateTime::from_timestamp(1791283680, 0).unwrap();
        let d = chat_test_body("discord", U, now);
        let e = &d["embeds"][0];
        assert_eq!(e["title"], "Test message from arcodash");
        assert_eq!(e["color"], 7566195);
        assert!(e.get("url").is_none());
        assert_eq!(e["fields"].as_array().unwrap().len(), 0);
        assert_eq!(
            e["description"],
            "Alerts for your monitors will arrive here, formatted like this."
        );
        let sl = chat_test_body("slack", U, now);
        assert_eq!(sl["attachments"][0]["color"], "#737373");
        assert!(sl["attachments"][0].get("title_link").is_none());
        let g = chat_test_body("google_chat", U, now);
        assert!(g["text"]
            .as_str()
            .unwrap()
            .starts_with("*Test message from arcodash*\n\n"));
    }

    #[test]
    fn no_explanation_and_no_ai_part() {
        let mut a = alert("triggered");
        a.explanation = None;
        let b = chat_alert_body("discord", U, &a);
        assert_eq!(
            b["embeds"][0]["description"],
            "The condition on this monitor is met."
        );
        assert_eq!(b["embeds"][0]["fields"].as_array().unwrap().len(), 2);
        a.explanation = Some("Only stats.".into());
        let b = chat_alert_body("slack", U, &a);
        assert_eq!(b["attachments"][0]["fields"].as_array().unwrap().len(), 2);
        assert!(b["attachments"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("Only stats."));
        a.explanation = Some("x\n\nAI analysis (confidence: low".into());
        let b = chat_alert_body("slack", U, &a);
        assert_eq!(b["attachments"][0]["fields"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn custom_copy_replaces_title_and_lead_only() {
        let mut a = alert("triggered");
        a.custom_subject = Some("Custom subj".into());
        a.custom_body = Some("Custom body".into());
        let b = chat_alert_body("slack", U, &a);
        let at = &b["attachments"][0];
        assert_eq!(at["title"], "Custom subj");
        assert_eq!(at["fallback"], "Custom subj");
        assert!(at["text"]
            .as_str()
            .unwrap()
            .starts_with("Custom body\n\nJumped"));
        assert_eq!(at["fields"].as_array().unwrap().len(), 3);
    }

    const HOSTILE: &str = "<b>*x*</b> @everyone <!channel> [x](http://evil)";

    fn hostile() -> ChatAlert {
        let mut a = alert("triggered");
        a.monitor_name = HOSTILE.into();
        a.custom_body = Some(HOSTILE.into());
        a.observed = Some(HOSTILE.into());
        a.explanation = Some(format!(
            "{HOSTILE}\n\nAI analysis (confidence: high): {HOSTILE}"
        ));
        a
    }

    #[test]
    fn slack_and_google_chat_entity_escape_everywhere() {
        for t in ["slack", "google_chat"] {
            let out = s(&chat_alert_body(t, U, &hostile()));
            assert!(!out.contains("<b>") && !out.contains("<!channel>"), "{t}");
            assert!(out.contains("&lt;!channel&gt;"), "{t}");
        }
    }

    #[test]
    fn mattermost_escapes_entities_markdown_and_mentions() {
        let out = s(&chat_alert_body("mattermost", U, &hostile()));
        assert!(!out.contains("<b>") && !out.contains("<!channel>"));
        assert!(!out.contains("@everyone"));
        assert!(out.contains("@\u{200B}everyone"));
        assert!(out.contains("\\\\[x\\\\]\\\\(http://evil\\\\)"));
    }

    #[test]
    fn discord_variants_escape_markdown() {
        for u in [U, DISCORD_SLACK] {
            let out = s(&chat_alert_body("discord", u, &hostile()));
            assert!(out.contains("\\\\*x\\\\*"));
            assert!(out.contains("\\\\[x\\\\]\\\\(http://evil\\\\)"));
        }
        let slack_compat = s(&chat_alert_body("discord", DISCORD_SLACK, &hostile()));
        assert!(!slack_compat.contains("@everyone"));
        assert!(slack_compat.contains("@\u{200B}everyone"));
    }

    #[test]
    fn teams_escapes_markdown_everywhere() {
        let out = s(&chat_alert_body("microsoft_teams_webhook", U, &hostile()));
        assert!(!out.contains("[x](http://evil)"));
        assert!(out.contains("\\\\[x\\\\]\\\\(http://evil\\\\)"));
        assert!(out.contains("\\\\*x\\\\*"));
    }

    #[test]
    fn discord_caps_hold_and_empty_fields_dropped() {
        let mut a = alert("triggered");
        a.monitor_name = "n".repeat(600);
        a.explanation = Some(format!(
            "{}\n\nAI analysis (confidence: high): {}",
            "é".repeat(5000),
            "*".repeat(3000)
        ));
        a.observed = Some("o".repeat(3000));
        a.condition = String::new();
        let b = chat_alert_body("discord", U, &a);
        let e = &b["embeds"][0];
        assert!(e["title"].as_str().unwrap().chars().count() <= 256);
        assert!(e["description"].as_str().unwrap().chars().count() <= 2000);
        let f = e["fields"].as_array().unwrap();
        assert_eq!(f.len(), 2);
        let mut total = e["title"].as_str().unwrap().chars().count()
            + e["description"].as_str().unwrap().chars().count();
        for x in f {
            let v = x["value"].as_str().unwrap().chars().count();
            assert!(v <= 1024 && v > 0);
            total += v + x["name"].as_str().unwrap().chars().count();
        }
        assert!(total < 6000);
        assert!(!e["fields"][1]["value"].as_str().unwrap().ends_with('\\'));
    }

    #[test]
    fn discord_slack_title_capped_and_link_syntax_defused() {
        let mut a = alert("triggered");
        a.monitor_name = "n".repeat(400);
        let b = chat_alert_body("discord", DISCORD_SLACK, &a);
        let t = b["attachments"][0]["title"].as_str().unwrap();
        assert!(t.chars().count() <= 256 && t.ends_with('…'));
        assert!(
            b["attachments"][0]["fallback"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 256
        );

        a.monitor_name = "<https://example.com|x>".into();
        let b = chat_alert_body("discord", DISCORD_SLACK, &a);
        let t = b["attachments"][0]["title"].as_str().unwrap();
        assert!(!t.contains('<'));
    }

    #[test]
    fn blank_custom_copy_uses_defaults() {
        let mut a = alert("triggered");
        a.custom_subject = Some("  ".into());
        a.custom_body = Some("\n".into());
        let b = chat_alert_body("discord", U, &a);
        assert!(!b["embeds"][0]["title"].as_str().unwrap().trim().is_empty());
        assert!(!b["embeds"][0]["description"]
            .as_str()
            .unwrap()
            .trim()
            .is_empty());
    }

    #[test]
    fn teams_text_blocks_capped() {
        let mut a = alert("triggered");
        a.custom_body = Some("b".repeat(10_000));
        a.explanation = Some("x".repeat(10_000));
        let b = chat_alert_body("microsoft_teams_webhook", U, &a);
        for blk in b["attachments"][0]["content"]["body"].as_array().unwrap() {
            if let Some(t) = blk["text"].as_str() {
                assert!(t.chars().count() <= 4000);
            }
        }
    }

    #[test]
    fn google_chat_text_capped() {
        let mut a = alert("triggered");
        a.explanation = Some("x".repeat(9000));
        let b = chat_alert_body("google_chat", U, &a);
        let t = b["text"].as_str().unwrap();
        assert!(t.chars().count() <= 4000);
        assert!(t.contains("*Condition:*"));
    }

    #[test]
    fn trunc_drops_dangling_backslash() {
        assert_eq!(trunc("ab\\cd", 4), "ab…");
        assert_eq!(trunc("abc", 3), "abc");
    }
}
