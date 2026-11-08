//! One HTML+text layout shared by every transactional email (monitor alerts, invitations) so
//! visual/structural changes land in exactly one place.
//!
//! Callers assemble already-worded content (`EmailLayout`) and this module only lays it out;
//! sending goes through [`crate::mailer::send_html_email`].

use std::fmt::Write as _;

/// One transactional email's content, fully worded by the caller.
pub struct EmailLayout<'a> {
    pub preheader: &'a str,
    pub heading: &'a str,
    /// Empty string omits the intro paragraph (a custom template replacing it entirely).
    pub intro: &'a str,
    pub facts: &'a [(&'a str, String)],
    /// Titled groups of (primary, secondary) rows rendered between the intro and the CTA.
    /// Empty for emails whose content fits the flat `facts` table.
    pub sections: &'a [(&'a str, &'a [(String, String)])],
    /// (label, url) — at most one per email.
    pub cta: Option<(&'a str, &'a str)>,
    /// (label, url) rendered as plain links below the CTA.
    pub secondary: &'a [(&'a str, String)],
    /// May contain embedded `\n` for a multi-line footer.
    pub footer: &'a str,
}

/// Every dynamic value flowing through this layout (monitor names, org names, invite emails) is
/// untrusted user input embedded into HTML — escape unconditionally rather than trusting callers.
///
/// `pub` so other transactional-email templates (e.g. an embedding app's own auth mail) share the
/// same escaping instead of a second copy.
#[must_use]
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Renders both MIME alternative parts for one email. Returns `(text, html)`.
#[must_use]
pub fn render(l: &EmailLayout<'_>) -> (String, String) {
    (render_text(l), render_html(l))
}

fn render_text(l: &EmailLayout<'_>) -> String {
    let mut out = String::new();
    let _ = write!(out, "{}\n\n", l.heading);
    if !l.intro.is_empty() {
        let _ = write!(out, "{}\n\n", l.intro);
    }

    // Labels right-padded to align every value at the same column, one space past the longest
    // "Label:" — matches the plain-text examples in the layout tests exactly.
    let colon_labels: Vec<String> = l
        .facts
        .iter()
        .map(|(label, _)| format!("{label}:"))
        .collect();
    let width = colon_labels
        .iter()
        .map(std::string::String::len)
        .max()
        .unwrap_or(0)
        + 1;
    for (colon_label, (_, value)) in colon_labels.iter().zip(l.facts.iter()) {
        let _ = writeln!(out, "{colon_label:<width$}{value}");
    }
    if !l.facts.is_empty() {
        out.push('\n');
    }

    for (title, rows) in l.sections {
        let _ = writeln!(out, "{title}");
        for (primary, secondary) in *rows {
            let _ = writeln!(out, "- {primary}");
            if !secondary.is_empty() {
                let _ = writeln!(out, "  {secondary}");
            }
        }
        out.push('\n');
    }

    if let Some((label, url)) = l.cta {
        let _ = write!(out, "{label}:\n{url}\n\n");
    }
    for (label, url) in l.secondary {
        let _ = write!(out, "{label}:\n{url}\n\n");
    }

    out.push_str(l.footer);
    out
}

fn render_html(l: &EmailLayout<'_>) -> String {
    let mut facts_rows = String::new();
    for (label, value) in l.facts {
        let _ = write!(
            facts_rows,
            r#"<tr><td style="color:#737373;padding:4px 12px 4px 0;white-space:nowrap;vertical-align:top;">{}</td><td style="color:#0a0a0a;padding:4px 0;">{}</td></tr>"#,
            escape_html(label),
            escape_html(value)
        );
    }

    let mut sections_html = String::new();
    for (title, rows) in l.sections {
        let _ = write!(
            sections_html,
            r#"<div style="margin-top:20px;font-size:11px;font-weight:600;letter-spacing:.07em;text-transform:uppercase;color:#737373;padding-bottom:6px;border-bottom:1px solid #e5e5e5;">{}</div>"#,
            escape_html(title)
        );
        sections_html.push_str(
            r#"<table role="presentation" width="100%" cellpadding="0" cellspacing="0">"#,
        );
        for (primary, secondary) in *rows {
            let secondary_html = if secondary.is_empty() {
                String::new()
            } else {
                format!(
                    r#"<div style="color:#737373;font-size:13px;">{}</div>"#,
                    escape_html(secondary)
                )
            };
            let _ = write!(
                sections_html,
                r#"<tr><td style="padding:10px 0;border-bottom:1px solid #f5f5f5;"><div style="color:#0a0a0a;font-weight:600;">{}</div>{}</td></tr>"#,
                escape_html(primary),
                secondary_html
            );
        }
        sections_html.push_str("</table>");
    }

    let cta_html = l
        .cta
        .map(|(label, url)| {
            let url_esc = escape_html(url);
            let label_esc = escape_html(label);
            format!(
                r#"<div style="margin:20px 0 4px;"><a href="{url_esc}" style="display:inline-block;background:#171717;color:#fafafa;text-decoration:none;padding:10px 18px;border-radius:6px;font-weight:500;font-size:14px;">{label_esc}</a></div><div style="font-size:12px;color:#737373;">{url_esc}</div>"#
            )
        })
        .unwrap_or_default();

    let mut secondary_html = String::new();
    for (label, url) in l.secondary {
        let _ = write!(
            secondary_html,
            r#"<div style="margin-top:12px;"><a href="{}" style="color:#7e14ff;text-decoration:underline;">{}</a></div>"#,
            escape_html(url),
            escape_html(label)
        );
    }

    let intro_html = if l.intro.is_empty() {
        String::new()
    } else {
        format!(
            r#"<p style="margin:0 0 16px;color:#0a0a0a;">{}</p>"#,
            escape_html(l.intro)
        )
    };

    let footer_html = escape_html(l.footer).replace('\n', "<br>");

    format!(
        r#"<span style="display:none;max-height:0;overflow:hidden;">{preheader}</span>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#fafafa;padding:24px 0;font-family:-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;font-size:14px;line-height:1.5;color:#0a0a0a;">
<tr><td align="center">
<table role="presentation" width="560" cellpadding="0" cellspacing="0" style="max-width:560px;width:100%;">
<tr><td style="padding:0 0 12px 4px;font-size:18px;font-weight:600;letter-spacing:.02em;"><span style="color:#7e14ff;">arc</span><span style="color:#171717;">odash</span></td></tr>
<tr><td style="background:#ffffff;border:1px solid #e5e5e5;border-radius:8px;padding:24px;">
<h1 style="margin:0 0 8px;font-size:18px;font-weight:600;color:#0a0a0a;">{heading}</h1>
{intro_html}
<table role="presentation" cellpadding="0" cellspacing="0">{facts_rows}</table>
{sections_html}
{cta_html}
{secondary_html}
<div style="border-top:1px solid #e5e5e5;padding-top:16px;margin-top:20px;color:#737373;font-size:12px;">{footer_html}</div>
</td></tr>
</table>
</td></tr>
</table>"#,
        preheader = escape_html(l.preheader),
        heading = escape_html(l.heading),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout<'a>(
        facts: &'a [(&'a str, String)],
        secondary: &'a [(&'a str, String)],
    ) -> EmailLayout<'a> {
        EmailLayout {
            preheader: "The condition on this monitor is met.",
            heading: "Revenue floor breach is alerting",
            intro: "The condition on this monitor is met.",
            facts,
            sections: &[],
            cta: Some(("View monitor", "https://app.arcodash.example/monitors/183")),
            secondary,
            footer: "You're receiving this because you subscribe to this monitor in arcodash.\nManage notifications: https://app.arcodash.example/settings/notifications",
        }
    }

    #[test]
    fn renders_both_parts_with_expected_shape() {
        let facts: Vec<(&str, String)> = vec![
            ("Monitor", "Revenue floor breach".to_string()),
            ("Query", "Net revenue by store and month".to_string()),
            ("Condition", "net_revenue is less than 20,000".to_string()),
            ("Observed", "12,480.55 (first row)".to_string()),
            ("Checked", "Aug 6, 2026, 2:48 AM UTC".to_string()),
            ("Previous state", "OK".to_string()),
        ];
        let secondary: Vec<(&str, String)> = vec![(
            "Open the query",
            "https://app.arcodash.example/queries/349".to_string(),
        )];
        let layout = sample_layout(&facts, &secondary);
        let (text, html) = render(&layout);

        assert!(text.starts_with("Revenue floor breach is alerting\n\n"));
        assert!(text.contains("Previous state: OK"));
        assert!(text.contains("View monitor:\nhttps://app.arcodash.example/monitors/183\n\n"));
        assert!(text.contains("Open the query:\nhttps://app.arcodash.example/queries/349\n\n"));
        assert!(text.ends_with(
            "Manage notifications: https://app.arcodash.example/settings/notifications"
        ));

        assert!(html.contains("<h1"));
        assert!(html.contains("Revenue floor breach is alerting"));
        assert!(html.contains("Previous state"));
        assert!(html.contains(r#"href="https://app.arcodash.example/monitors/183""#));
        assert!(html.contains(r#"href="https://app.arcodash.example/queries/349""#));
    }

    #[test]
    fn fact_labels_align_to_the_longest_label_plus_one_space() {
        let facts: Vec<(&str, String)> = vec![
            ("Monitor", "Revenue floor breach".to_string()),
            ("Previous state", "OK".to_string()),
        ];
        let layout = sample_layout(&facts, &[]);
        let (text, _) = render(&layout);
        assert!(text.contains("Monitor:        Revenue floor breach\n"));
        assert!(text.contains("Previous state: OK\n"));
    }

    #[test]
    fn html_escapes_untrusted_values() {
        let facts: Vec<(&str, String)> = vec![("Monitor", "<script>alert(1)</script>".to_string())];
        let layout = EmailLayout {
            heading: "<b>bold</b> heading",
            ..sample_layout(&facts, &[])
        };
        let (text, html) = render(&layout);
        // The text part is never HTML-stripped — markup travels through verbatim.
        assert!(text.contains("<script>alert(1)</script>"));
        assert!(text.contains("<b>bold</b> heading"));
        // The HTML part must never carry an unescaped tag from user-controlled content.
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<b>bold</b>"));
        assert!(html.contains("&lt;b&gt;bold&lt;/b&gt;"));
    }

    #[test]
    fn escape_html_covers_all_five_special_characters() {
        assert_eq!(escape_html(r#"<a>&"'"#), "&lt;a&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn escape_html_is_noop_on_plain_text() {
        assert_eq!(escape_html("Alice Example"), "Alice Example");
    }

    #[test]
    fn non_ascii_survives_both_parts_untouched() {
        let facts: Vec<(&str, String)> = vec![("Note", "café — naïve résumé €".to_string())];
        let layout = EmailLayout {
            heading: "café — naïve résumé €",
            ..sample_layout(&facts, &[])
        };
        let (text, html) = render(&layout);
        assert!(text.contains("café — naïve résumé €"));
        assert!(html.contains("café — naïve résumé €"));
    }

    #[test]
    fn empty_intro_is_omitted_from_both_parts() {
        let facts: Vec<(&str, String)> = vec![("Monitor", "x".to_string())];
        let layout = EmailLayout {
            intro: "",
            ..sample_layout(&facts, &[])
        };
        let (text, html) = render(&layout);
        assert!(text.starts_with("Revenue floor breach is alerting\n\nMonitor:"));
        assert!(!html.contains("<p "));
    }

    #[test]
    fn empty_sections_render_nothing_in_either_part() {
        let facts: Vec<(&str, String)> = vec![
            ("Monitor", "Revenue floor breach".to_string()),
            ("Previous state", "OK".to_string()),
        ];
        let secondary: Vec<(&str, String)> = vec![(
            "Open the query",
            "https://app.arcodash.example/queries/349".to_string(),
        )];
        let layout = sample_layout(&facts, &secondary);
        let (text, html) = render(&layout);

        assert!(text.starts_with("Revenue floor breach is alerting\n\n"));
        assert!(text.contains("Previous state: OK"));
        assert!(text.contains("View monitor:\nhttps://app.arcodash.example/monitors/183\n\n"));
        assert!(text.contains("Open the query:\nhttps://app.arcodash.example/queries/349\n\n"));
        assert!(text.ends_with(
            "Manage notifications: https://app.arcodash.example/settings/notifications"
        ));

        assert!(!html.contains("text-transform:uppercase"));
        assert!(!html.contains("letter-spacing:.07em"));
    }

    #[test]
    fn sections_render_titles_and_rows_in_both_parts() {
        let rows_a: Vec<(String, String)> = vec![
            (
                "<script>alert(1)</script>".to_string(),
                "OK → Alerting · Aug 6, 2026, 2:48 AM UTC".to_string(),
            ),
            ("Second Monitor".to_string(), String::new()),
        ];
        let rows_b: Vec<(String, String)> = vec![(
            "Third Monitor".to_string(),
            "Since Aug 5, 2026, 1:00 PM UTC".to_string(),
        )];
        let sections: Vec<(&str, &[(String, String)])> =
            vec![("<b>Changes</b>", &rows_a), ("Currently Alerting", &rows_b)];
        let facts: Vec<(&str, String)> = vec![];
        let layout = EmailLayout {
            sections: &sections,
            ..sample_layout(&facts, &[])
        };
        let (text, html) = render(&layout);

        assert!(text.contains("<b>Changes</b>\n"));
        assert!(text.contains("- <script>alert(1)</script>\n"));
        assert!(text.contains("  OK → Alerting · Aug 6, 2026, 2:48 AM UTC\n"));
        assert!(text.contains("- Second Monitor\n"));
        assert!(!text.contains("Second Monitor\n  "));
        assert!(text.contains("Currently Alerting\n"));
        assert!(text.contains("- Third Monitor\n"));
        assert!(text.contains("  Since Aug 5, 2026, 1:00 PM UTC\n"));

        assert!(html.contains("&lt;b&gt;Changes&lt;/b&gt;"));
        assert!(!html.contains("<b>Changes</b>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("OK → Alerting"));
        assert!(html.contains("Third Monitor"));
        assert!(html.contains("Since Aug 5, 2026, 1:00 PM UTC"));
    }
}
