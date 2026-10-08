//! Authored significance, with historical references kept separate from live reads.
use super::{DIM, GOLD, INK, LILAC, MINT, PANEL, s, short, single, text_line, timestamp};
use crate::model::{App, clean};
use crate::touchstones::ReferenceResolution;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Text},
    widgets::{Paragraph, Wrap},
};

fn centered(area: Rect) -> Rect {
    let width = area.width.min(112);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y,
        width,
        area.height,
    )
}

/// Show where an account came from, not the storage codec and replay digest.
/// The complete native provenance remains in the retained snapshot.
fn source_label(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return single(raw);
    };
    if let Some(source) = value.get("External").and_then(|v| v.get("source")) {
        if let (Some(namespace), Some(reference)) =
            (source["namespace"].as_str(), source["reference"].as_str())
        {
            return format!("{} · {}", single(namespace), single(reference));
        }
    }
    if let Some(web) = value.get("Web") {
        if let Some(url) = web["url"].as_str() {
            return format!("web · {}", single(url));
        }
    }
    if let Some(conversation) = value.get("Conversation") {
        if let (Some(session), Some(turn)) = (
            conversation["session"].as_str(),
            conversation["turn"].as_u64(),
        ) {
            return format!("conversation · {} · turn {turn}", single(session));
        }
    }
    if let Some(sources) = value.get("Derived").and_then(|v| v["from"].as_array()) {
        return format!(
            "derived · {} source memories (identities in native GET)",
            sources.len()
        );
    }
    single(raw)
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let area = centered(area);
    if area.width >= 88 {
        let left = area.width * 2 / 5;
        cards(frame, Rect::new(area.x, area.y, left, area.height), app);
        inspector(
            frame,
            Rect::new(
                area.x + left + 2,
                area.y,
                area.width.saturating_sub(left + 2),
                area.height,
            ),
            app,
        );
    } else if app.cabinet.detail.is_some() {
        inspector(frame, area, app);
    } else {
        cards(frame, area, app);
    }
}
fn cards(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cabinet = &app.cabinet;
    let mut lines = vec![
        Line::from(s("TOUCHSTONES", LILAC)),
        Line::from(s("Authored meaning · not a ranking", DIM)),
        Line::default(),
    ];
    if cabinet.page.items.is_empty() {
        lines.push(Line::from(s(
            if !cabinet.loaded {
                if app.busy {
                    "Reading this source…"
                } else if app.error.is_some() {
                    "Touchstones unavailable · r retries"
                } else {
                    "Not loaded · r reads this source"
                }
            } else {
                "No touchstones on this page."
            },
            DIM,
        )));
    } else {
        let rows = usize::from(area.height.saturating_sub(5)) / 3;
        let rows = rows.max(1);
        let start = cabinet
            .selected
            .saturating_sub(rows / 2)
            .min(cabinet.page.items.len().saturating_sub(rows));
        for (index, card) in cabinet.page.items.iter().enumerate().skip(start).take(rows) {
            let chosen = index == cabinet.selected;
            lines.push(Line::from(s(
                format!(
                    "{}{}",
                    if chosen { "› " } else { "  " },
                    short(&card.summary, usize::from(area.width.saturating_sub(2)))
                ),
                if chosen { MINT } else { INK },
            )));
            lines.push(Line::from(s(
                short(
                    &format!(
                        "  {} · {} refs · {}",
                        single(&card.subject),
                        card.reference_count,
                        single(&card.status)
                    ),
                    usize::from(area.width),
                ),
                DIM,
            )));
            lines.push(Line::default());
        }
    }
    let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        body,
    );
    if area.height > 0 {
        let coverage = if cabinet.loaded {
            format!(
                "{} · {}{}",
                cabinet.page.items.len(),
                if cabinet.page.next.is_some() {
                    "n next"
                } else {
                    "end"
                },
                if cabinet.page.partial {
                    " · partial"
                } else {
                    ""
                }
            )
        } else if app.busy {
            "reading…".into()
        } else if app.error.is_some() {
            "unavailable · r retry".into()
        } else {
            "not loaded · r read".into()
        };
        text_line(
            frame,
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            Line::from(s(short(&coverage, usize::from(area.width)), GOLD)),
        );
    }
}
fn inspector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(detail) = &app.cabinet.detail {
        lines.extend([
            Line::from(s(single(&detail.node.summary), MINT)),
            Line::from(s(
                format!("{} · {}", single(&detail.owner), single(&detail.subject)),
                DIM,
            )),
            Line::default(),
            Line::from(s("ANNOTATION", LILAC)),
        ]);
        if detail.summary_partial {
            lines.insert(
                1,
                Line::from(s("… annotation summary excerpt · owner-truncated", GOLD)),
            );
        }
        lines.extend(
            clean(&detail.node.body)
                .lines()
                .map(|line| Line::from(s(line, INK))),
        );
        if detail.body_partial {
            lines.push(Line::from(s("… annotation excerpt · byte-bounded", GOLD)));
        }
        lines.push(Line::default());
        if let Some(reference) = detail.references.get(app.cabinet.reference) {
            lines.push(Line::from(s(
                format!(
                    "HISTORICAL SNAPSHOT  {}/{} · [ / ] choose",
                    app.cabinet.reference + 1,
                    detail.references.len()
                ),
                LILAC,
            )));
            lines.push(Line::from(s(single(&reference.summary), INK)));
            lines.push(Line::from(s(
                format!("{} / {}", single(&reference.db_id), single(&reference.id)),
                DIM,
            )));
            lines.push(Line::from(s(
                format!(
                    "{} · created {}",
                    single(&reference.memory_kind),
                    timestamp(reference.created)
                ),
                DIM,
            )));
            lines.push(Line::from(s(
                format!("source  {}", source_label(&reference.provenance)),
                DIM,
            )));
            let status = match reference.resolution {
                ReferenceResolution::Unchanged => "Snapshot fields unchanged · body not compared",
                ReferenceResolution::SnapshotChanged => {
                    "Snapshot fields changed · historical summary retained"
                }
                ReferenceResolution::Absent => "Exact target absent · historical summary retained",
                ReferenceResolution::Unavailable => "Owner unavailable · not evidence of deletion",
            };
            lines.push(Line::from(s(status, GOLD)));
            lines.push(Line::from(s("summary_only · no archived body", DIM)));
            lines.push(Line::default());
            if let Some(current) = &app.cabinet.current {
                lines.push(Line::from(s(
                    "EXACT TARGET TODAY · explicit current read",
                    MINT,
                )));
                lines.push(Line::from(s(
                    format!("owner {}", single(&current.db_id)),
                    DIM,
                )));
                if let Some(node) = &current.node {
                    lines.push(Line::from(s(single(&node.summary), INK)));
                    if current.summary_partial {
                        lines.push(Line::from(s(
                            "… current summary excerpt · owner-truncated",
                            GOLD,
                        )));
                    }
                    lines.push(Line::from(s(
                        format!("status  {}", single(&node.status)),
                        GOLD,
                    )));
                    lines.extend(
                        clean(&node.body)
                            .lines()
                            .map(|line| Line::from(s(line, INK))),
                    );
                    if current.body_partial {
                        lines.push(Line::from(s("… current body excerpt · byte-bounded", GOLD)));
                    }
                } else {
                    lines.push(Line::from(s(
                        "Exact target absent · historical snapshot above retained",
                        GOLD,
                    )));
                }
            } else {
                lines.push(Line::from(s(
                    "c reads this exact target today · never a successor",
                    DIM,
                )));
            }
        } else {
            lines.push(Line::from(s("No references attached.", DIM)));
        }
    } else if let Some(card) = app.cabinet.page.items.get(app.cabinet.selected) {
        lines.extend([
            Line::from(s(single(&card.summary), MINT)),
            Line::default(),
            Line::from(s(format!("subject  {}", single(&card.subject)), LILAC)),
            Line::from(s(
                format!(
                    "{} references · {}",
                    card.reference_count,
                    single(&card.status)
                ),
                DIM,
            )),
            Line::from(s(format!("annotation  {}", single(&card.id)), DIM)),
            Line::default(),
            Line::from(s("Enter / i reads the annotation body.", DIM)),
        ]);
    } else {
        lines.extend([
            Line::from(s("A cabinet of deliberate significance.", MINT)),
            Line::default(),
            Line::from(s("Enter / i reads the annotation.", DIM)),
            Line::from(s(
                "References retain historical summaries; current bodies require an explicit read.",
                DIM,
            )),
        ]);
    }
    if app.cabinet.detail.is_some() {
        crate::detail_scroll::draw(frame, area, app, lines, Style::default().bg(PANEL));
    } else {
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(PANEL))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_label_keeps_the_source_not_the_replay_codec() {
        let raw = serde_json::json!({"External":{"source":{
            "namespace":"manual", "reference":"manual-submission:scene-1",
            "key":"scene-1", "request_codec":"episode_v1", "request_digest":[1,2,3]
        }}})
        .to_string();
        assert_eq!(source_label(&raw), "manual · manual-submission:scene-1");
        assert_eq!(
            source_label(r#"{"Conversation":{"session":"session-1","turn":3}}"#),
            "conversation · session-1 · turn 3"
        );
        assert_eq!(source_label("Synthetic\u{1b} example"), "Synthetic example");
    }
    #[test]
    fn cabinet_width_is_balanced_and_bounded() {
        assert_eq!(
            centered(Rect::new(1, 2, 160, 20)),
            Rect::new(25, 2, 112, 20)
        );
        assert_eq!(centered(Rect::new(1, 2, 60, 20)), Rect::new(1, 2, 60, 20));
    }
    fn plain(width: u16, height: u16, app: &App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn snapshot_and_current_body_stay_distinct_in_full_and_narrow_inspectors() {
        let mut app = App::new(vec![], String::new(), true);
        app.view = crate::model::View::Touchstones;
        app.cabinet.page = crate::touchstones::demo_touchstones();
        app.cabinet.loaded = true;
        let mut detail = crate::touchstones::demo_detail(&app.cabinet.page.items[0]);
        detail.references[0].resolution = ReferenceResolution::Unchanged;
        detail.references[0].summary = "Historical\x1b summary".into();
        app.cabinet.detail = Some(detail);
        app.cabinet.current = Some(crate::touchstones::ExactTarget {
            db_id: "owner".into(),
            node: Some(crate::model::Node {
                summary: "Current target".into(),
                status: "Archived".into(),
                body: "A body read today".into(),
                ..Default::default()
            }),
            body_partial: false,
            summary_partial: false,
        });
        for width in [120, 60] {
            let text = plain(width, 40, &app);
            assert!(text.contains("Historical summary"), "{text}");
            assert!(text.contains("body not compared"), "{text}");
            assert!(text.contains("summary_only"), "{text}");
            assert!(text.contains("EXACT TARGET TODAY"), "{text}");
            assert!(text.contains("A body read today"), "{text}");
            assert!(text.contains("status  Archived"), "{text}");
            assert!(!text.contains('\x1b'));
        }
        app.cabinet.current.as_mut().unwrap().node = None;
        assert!(plain(60, 40, &app).contains("historical snapshot above retained"));
    }
    #[test]
    fn summary_excerpts_are_distinct_from_body_coverage() {
        let mut app = App::new(vec![], String::new(), true);
        app.view = crate::model::View::Touchstones;
        let card = crate::touchstones::demo_touchstones().items.remove(0);
        let mut detail = crate::touchstones::demo_detail(&card);
        detail.summary_partial = true;
        app.cabinet.detail = Some(detail);
        app.cabinet.current = Some(crate::touchstones::ExactTarget {
            db_id: "owner".into(),
            node: Some(crate::model::Node {
                summary: "A cropped summary".into(),
                body: "An uncropped body".into(),
                ..Default::default()
            }),
            body_partial: false,
            summary_partial: true,
        });
        for width in [120, 60] {
            let text = plain(width, 60, &app);
            assert!(text.contains("annotation summary excerpt"), "{text}");
            assert!(text.contains("current summary excerpt"), "{text}");
            assert!(text.contains("An uncropped body"), "{text}");
            assert!(!text.contains("current body excerpt"), "{text}");
        }
    }
    #[test]
    fn continuation_and_partial_markers_survive_tiny_shelf() {
        let mut app = App::new(vec![], String::new(), true);
        app.view = crate::model::View::Touchstones;
        app.cabinet.page = crate::touchstones::demo_touchstones();
        app.cabinet.loaded = true;
        app.cabinet.page.next = Some("cursor".into());
        app.cabinet.page.partial = true;
        assert!(plain(24, 9, &app).contains("n next · partial"));
        app.cabinet.page.items[0].summary = "A full selected summary beyond shelf preview".into();
        assert!(plain(120, 36, &app).contains("beyond shelf preview"));
    }
    #[test]
    fn normal_narrow_tiny_and_control_text_render_safely() {
        let mut app = App::new(vec![], String::new(), true);
        app.view = crate::model::View::Touchstones;
        for (width, height) in [(120, 36), (60, 24), (24, 9), (10, 4)] {
            let text = super::super::preview(width, height, &app).unwrap();
            assert!(!text.is_empty());
        }
        assert!(!single("name\x1b[31m\r\nnext").contains(['\x1b', '\r', '\n']));
    }
}
