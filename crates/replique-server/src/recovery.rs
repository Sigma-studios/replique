//! The recovery plan and the paper kit's checklist.
//!
//! The plan says where every secret lives and what to do in each disaster; the
//! checklist names what is written by hand in the kit envelopes. Neither holds
//! a secret: Réplique is reachable from the internet behind one password, the
//! last place recovery codes belong. The printed kit (`/recovery/print`) is the
//! copy used the day everything is down, so Réplique reminds until it is
//! printed and filled in, again when the plan or the checklist changes, and
//! once a year to check both envelopes. The plan and the checklist also go to
//! Réplique's area on the Storage Box, readable with Hetzner access alone.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use maud::{Markup, PreEscaped};

use crate::App;
use crate::alerts::{self, Raise};
use crate::db;

const PLAN: &str = "recovery_plan";
const VERSION: &str = "recovery_version";
const PRINTED: &str = "recovery_printed_version";
const REVIEWED: &str = "recovery_reviewed_at";
const PINGED: &str = "recovery_pinged_at";
const ON_BOX: &str = "recovery_box_version";
const SEEDED: &str = "recovery_seeded";
const ALERT_KEY: &str = "recovery_pending";

const YEAR: i64 = 365 * 86_400;
const WEEK: i64 = 7 * 86_400;

const DEFAULT_PLAN: &str = include_str!("recovery_plan.md");

/// What the kit should hold to begin with: name, and where to find it.
const DEFAULT_ENTRIES: &[(&str, &str)] = &[
    (
        "Bitwarden: two-step login recovery code",
        "Bitwarden web vault → Settings → Security → Two-step login → View recovery code",
    ),
    (
        "Aegis password",
        "The password of the Aegis app and of its backups (also in my head)",
    ),
    (
        "VPS provider (façade, mail): 2FA recovery codes",
        "The provider account of the server running façade and the mail",
    ),
    ("façade admin password", "facade CLI and web UI"),
    (
        "façade offsite key",
        "Shown by: facade backup offsite key. Rebuilds the server from its offsite copy",
    ),
    (
        "Hetzner: 2FA recovery codes",
        "Hetzner Console → account → security",
    ),
    (
        "Porkbun: 2FA recovery codes",
        "Porkbun → Account → security",
    ),
    ("GitHub: recovery codes", "github.com/settings/security"),
    (
        "Réplique admin password",
        "To download the Bitwarden copies from the Recovery page",
    ),
];

fn int(app: &App, key: &str) -> i64 {
    app.store
        .setting_str(key)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn set_int(app: &App, key: &str, v: i64) -> anyhow::Result<()> {
    app.store.put_setting(key, v.to_string().as_bytes())
}

/// The first draft and the default checklist, once.
pub fn seed(app: &App) -> anyhow::Result<()> {
    if app.store.setting(SEEDED)?.is_some() {
        return Ok(());
    }
    app.store.put_setting(PLAN, DEFAULT_PLAN.as_bytes())?;
    for (name, hint) in DEFAULT_ENTRIES {
        app.store.add_recovery_entry(name, hint)?;
    }
    set_int(app, VERSION, 1)?;
    app.store.put_setting(SEEDED, b"1")
}

pub fn plan(app: &App) -> String {
    app.store
        .setting_str(PLAN)
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub fn version(app: &App) -> i64 {
    int(app, VERSION)
}

pub fn printed_version(app: &App) -> i64 {
    int(app, PRINTED)
}

pub fn reviewed_at(app: &App) -> Option<i64> {
    Some(int(app, REVIEWED)).filter(|t| *t > 0)
}

/// The plan or the checklist changed: the envelopes are out of date.
pub fn bump(app: &App) -> anyhow::Result<()> {
    set_int(app, VERSION, version(app) + 1)
}

pub fn set_plan(app: &App, text: &str) -> anyhow::Result<()> {
    if text == plan(app) {
        return Ok(());
    }
    app.store.put_setting(PLAN, text.as_bytes())?;
    bump(app)
}

/// Printed and filled in: the envelopes hold this version.
pub fn mark_printed(app: &App) -> anyhow::Result<()> {
    set_int(app, PRINTED, version(app))?;
    set_int(app, REVIEWED, db::now())
}

/// Both envelopes checked (the yearly review).
pub fn mark_reviewed(app: &App) -> anyhow::Result<()> {
    set_int(app, REVIEWED, db::now())
}

/// What is left to do, in words; empty when the kit is up to date.
pub fn pending(app: &App) -> Vec<String> {
    let mut out = Vec::new();
    let entries = app.store.recovery_entries().unwrap_or_default();
    let unwritten: Vec<&str> = entries
        .iter()
        .filter(|e| e.written_at.is_none())
        .map(|e| e.name.as_str())
        .collect();
    if !unwritten.is_empty() {
        out.push(format!("Not written down yet: {}.", unwritten.join(", ")));
    }
    let (v, p) = (version(app), printed_version(app));
    if p == 0 {
        out.push("The kit has never been printed.".into());
    } else if p < v {
        out.push(format!(
            "The plan or the checklist changed since the kit was printed (version {p} in the envelopes, {v} now): print it again."
        ));
    }
    if p > 0 && reviewed_at(app).is_none_or(|t| db::now() - t > YEAR) {
        out.push("Yearly review: check both envelopes are where the plan says, their seals intact, and every entry still current.".into());
    }
    out
}

/// A Problem while something is left to do, with a Discord reminder every week
/// until it is done.
pub async fn remind(app: &Arc<App>) {
    let todo = pending(app);
    if todo.is_empty() {
        alerts::clear(app, ALERT_KEY).await;
        return;
    }
    let open = app.store.open_alert(ALERT_KEY).ok().flatten().is_some();
    if !open {
        alerts::raise(
            app,
            Raise {
                key: ALERT_KEY.into(),
                kind: "recovery",
                severity: alerts::WARNING,
                title: "Recovery kit: to do".into(),
                detail: todo.join("\n"),
                device_id: None,
            },
        )
        .await;
        let _ = set_int(app, PINGED, db::now());
    } else if db::now() - int(app, PINGED) >= WEEK {
        let text = format!(
            "**Reminder: recovery kit**\n{}\n{}/recovery",
            todo.join("\n"),
            app.config.public_url.trim_end_matches('/')
        );
        if let Err(e) = alerts::notify(app, &text, false).await {
            tracing::warn!("Discord: {e:#}");
        }
        let _ = set_int(app, PINGED, db::now());
    }
}

/// The checklist as text, for the box: names, hints and dates, no values.
pub fn kit_markdown(app: &App) -> String {
    let mut s = format!(
        "# Recovery kit checklist (version {})\n\nWhat is written by hand in the kit envelopes. The values are only on paper.\n\n",
        version(app)
    );
    for e in app.store.recovery_entries().unwrap_or_default() {
        let written = e
            .written_at
            .map(crate::web::fmt::datetime)
            .unwrap_or_else(|| "not yet".into());
        s.push_str(&format!(
            "- **{}**: {} (written: {written})\n",
            e.name, e.hint
        ));
    }
    s
}

/// The plan and the checklist go to the box whenever they changed. Tried from
/// the periodic checks, at most every ten minutes after a failure.
pub async fn sync_box(app: &Arc<App>) {
    static LAST_FAILURE: AtomicI64 = AtomicI64::new(0);
    let v = version(app);
    if v == 0 || int(app, ON_BOX) >= v || db::now() - LAST_FAILURE.load(Ordering::Relaxed) < 600 {
        return;
    }
    let result = async {
        crate::vault::write_to_area(app, "recovery/plan.md", plan(app).as_bytes()).await?;
        crate::vault::write_to_area(app, "recovery/kit.md", kit_markdown(app).as_bytes()).await?;
        set_int(app, ON_BOX, v)
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("copying the recovery plan to the Storage Box: {e:#}");
        LAST_FAILURE.store(db::now(), Ordering::Relaxed);
    }
}

/// Markdown to HTML. Raw HTML in the text is shown as text: the plan is the
/// admin's own, but nothing typed into a page should become markup.
pub fn render(text: &str) -> Markup {
    use pulldown_cmark::{Event, Options, Parser, html};
    let parser =
        Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(|e| {
            match e {
                Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
                other => other,
            }
        });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    PreEscaped(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_escapes_html() {
        let r = render("# Plan\n\n<script>alert(1)</script>\n\n| a | b |\n| - | - |\n| 1 | 2 |")
            .into_string();
        assert!(r.contains("<h1>Plan</h1>"));
        assert!(r.contains("&lt;script&gt;"));
        assert!(r.contains("<table>"));
    }
}
