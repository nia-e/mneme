//! A view of the shared resolver's evidence, never another policy resolver.
//! No action here opens an owner. Existing in-flight reads may finish, but the
//! event loop admits no new memory reads while this modal is displayed.
use crate::model::{App, clean};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

const INK: Color = Color::Rgb(226, 235, 239);
const MINT: Color = Color::Rgb(119, 237, 192);
const DIM: Color = Color::Rgb(113, 137, 153);
const GOLD: Color = Color::Rgb(233, 197, 129);
const PANEL: Color = Color::Rgb(16, 23, 35);

pub(crate) struct State {
    selected: usize,
    scroll: usize,
    viewport: RefCell<Option<Viewport>>,
    hits: RefCell<Vec<(Rect, usize)>>,
}

#[derive(Clone, Copy)]
struct Viewport {
    area: Rect,
    max: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Input {
    Pass,
    Consumed,
    Quit,
}

pub(crate) fn open(app: &mut App) {
    let id = app
        .source_menu
        .and_then(|index| app.sources.get(index))
        .and_then(|source| source.expected_db_id.as_deref());
    let selected = id
        .and_then(|id| rows(app).iter().position(|(candidate, _)| *candidate == id))
        .map_or(0, |index| index + 1);
    app.config_inspector = Some(State {
        selected,
        scroll: 0,
        viewport: RefCell::default(),
        hits: RefCell::default(),
    });
    app.show_help = false;
    app.layout.borrow_mut().cancel_drag();
}

pub(crate) fn clamp(app: &mut App) {
    if let Some(state) = &mut app.config_inspector {
        if let Some(view) = *state.viewport.borrow() {
            state.scroll = state.scroll.min(view.max);
        }
    }
}

/// Arrow left/right changes the selected store even in a narrow terminal;
/// up/down always scrolls evidence. Enter deliberately does not open a source.
pub(crate) fn input(app: &mut App, event: &Event, wheel_reports: usize) -> Input {
    if app.config_inspector.is_none() {
        return Input::Pass;
    }
    let count = rows(app).len() + 1;
    let state = app.config_inspector.as_mut().unwrap();
    let view = *state.viewport.borrow();
    let max = view.map_or(0, |view| view.max);
    let page = view.map_or(1, |view| {
        usize::from(view.area.height).saturating_sub(1).max(1)
    });
    state.scroll = state.scroll.min(max);
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            if key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Input::Quit;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('C') | KeyCode::Char('c') => {
                    app.config_inspector = None;
                }
                KeyCode::Right | KeyCode::Tab | KeyCode::Char(']') => {
                    state.selected = (state.selected + 1) % count;
                    state.scroll = 0;
                }
                KeyCode::Left | KeyCode::BackTab | KeyCode::Char('[') => {
                    state.selected = (state.selected + count - 1) % count;
                    state.scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.scroll = state.scroll.saturating_add(1).min(max)
                }
                KeyCode::Up | KeyCode::Char('k') => state.scroll = state.scroll.saturating_sub(1),
                KeyCode::PageDown => state.scroll = state.scroll.saturating_add(page).min(max),
                KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(page),
                KeyCode::Home => state.scroll = 0,
                KeyCode::End => state.scroll = max,
                _ => {}
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let selected = state
                    .hits
                    .borrow()
                    .iter()
                    .find(|(area, _)| area.contains((mouse.column, mouse.row).into()))
                    .map(|(_, index)| *index);
                if let Some(index) = selected {
                    state.selected = index;
                    state.scroll = 0;
                }
            }
            MouseEventKind::ScrollDown
                if view
                    .is_some_and(|view| view.area.contains((mouse.column, mouse.row).into())) =>
            {
                state.scroll = state
                    .scroll
                    .saturating_add(wheel_reports.max(1).saturating_mul(3))
                    .min(max);
            }
            MouseEventKind::ScrollUp
                if view
                    .is_some_and(|view| view.area.contains((mouse.column, mouse.row).into())) =>
            {
                state.scroll = state
                    .scroll
                    .saturating_sub(wheel_reports.max(1).saturating_mul(3));
            }
            _ => {}
        },
        // Geometry is redrawn next turn; no underlying view receives the event.
        _ => {}
    }
    Input::Consumed
}

fn rows(app: &App) -> Vec<(&str, &str)> {
    if let Some(report) = &app.config_report {
        report
            .stores
            .iter()
            .map(|(id, row)| (id.as_str(), row.label.as_str()))
            .collect()
    } else if let Some(selection) = &app.config_selection {
        selection
            .stores
            .iter()
            .map(|row| (row.store.id.as_str(), row.name.as_str()))
            .collect()
    } else {
        // Explicit remote/library selection is deliberately isolated from ambient
        // discovery. Its configured source metadata remains available in Tab.
        vec![]
    }
}

fn value(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(|| "not present in report".into(), |value| value.to_string())
}

fn text(lines: &mut Vec<Line<'static>>, value: impl AsRef<str>) {
    lines.push(Line::raw(clean(value.as_ref())));
}

fn heading(lines: &mut Vec<Line<'static>>, value: impl Into<String>) {
    lines.push(Line::styled(
        clean(&value.into()),
        Style::default().fg(MINT),
    ));
}

/// Snapshot manifests record whole Unix seconds, unlike memory timestamps.
pub(crate) fn snapshot_age(captured_at: u64) -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
        |_| "unknown (clock before Unix epoch)".into(),
        |now| snapshot_age_at(captured_at, now.as_secs()),
    )
}

fn snapshot_age_at(captured_at: u64, now: u64) -> String {
    let seconds = now.abs_diff(captured_at);
    let duration = if seconds >= 86_400 {
        format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600)
    } else if seconds >= 3_600 {
        format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    };
    if captured_at > now {
        format!("future by {duration} (clock skew)")
    } else {
        format!("{duration} old")
    }
}

fn read_source(lines: &mut Vec<Line<'static>>, source: &crate::read_source::ReadProvenance) {
    heading(lines, "Displayed read source");
    let Some(snapshot) = &source.snapshot else {
        text(
            lines,
            "Read source: unknown (no snapshot witness retained).",
        );
        return;
    };
    if source.mixed {
        text(
            lines,
            "Mode: MIXED reads; this SNAPSHOT component is read-only.",
        );
        text(
            lines,
            "This witness does not describe every displayed read.",
        );
    } else {
        text(lines, "Mode: SNAPSHOT · read-only");
    }
    text(
        lines,
        format!("Canonical owner ID: {}", snapshot.owner_db_id),
    );
    text(
        lines,
        format!(
            "Captured UTC: {}",
            mneme_present::format_timestamp_millis(u128::from(snapshot.captured_at) * 1_000)
        ),
    );
    text(
        lines,
        format!("Age: {}", snapshot_age(snapshot.captured_at)),
    );
    text(
        lines,
        format!("Source device: {}", snapshot.source_device_id),
    );
    text(lines, format!("Generation: {}", snapshot.generation));
    text(lines, format!("Snapshot database: {}", snapshot.database));
    text(
        lines,
        format!(
            "Resolved snapshot path: {}",
            snapshot.resolved_path.display()
        ),
    );
    text(
        lines,
        "Retained read evidence; owner activity is not freshness proof.",
    );
}

fn overview(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![];
    heading(&mut lines, "Configuration evidence");
    read_source(&mut lines, &app.displayed_read_source());
    if app.demo {
        text(
            &mut lines,
            "DEMO: synthetic configuration; no owner contact.",
        );
    }
    if !app.config_diagnostics.is_empty() || app.error.is_some() {
        heading(&mut lines, "Startup / source diagnostics");
        if let Some(error) = &app.error {
            text(&mut lines, error);
        }
        for event in &app.config_diagnostics {
            text(&mut lines, event);
        }
    }
    if let Some(selection) = &app.config_selection {
        if selection.purpose == mneme_config::SelectionPurpose::Browse {
            text(&mut lines, "Selection: explicit read-only browsing");
            text(
                &mut lines,
                "Workspace applicability is ignored; automatic memory is unchanged.",
            );
            text(
                &mut lines,
                "Discovery diagnostics below describe automatic policy, not owner reachability.",
            );
        }
        text(&mut lines, format!("Profile: {}", selection.profile));
        text(
            &mut lines,
            format!(
                "Read default: {}",
                selection.read_default.as_deref().unwrap_or("none admitted")
            ),
        );
        text(
            &mut lines,
            format!("Selection digest: {}", selection.digest),
        );
        if !selection.sources.is_empty() {
            heading(&mut lines, "Local configuration sources");
            for source in &selection.sources {
                text(
                    &mut lines,
                    format!("{}: {}", source.role, source.path.display()),
                );
                text(
                    &mut lines,
                    format!("  SHA256: {} · {} bytes", source.sha256, source.bytes),
                );
            }
        }
    } else {
        text(
            &mut lines,
            "Admission / activation: not resolved (no selection snapshot).",
        );
    }
    if let Some(report) = &app.config_report {
        text(
            &mut lines,
            format!(
                "{} stores · {} conflicts · {} unavailable publishers",
                report.stores.len(),
                report.conflicts.len(),
                report.unavailable.len()
            ),
        );
        heading(&mut lines, "Publisher snapshots");
        for (id, revision) in &report.publisher_revisions {
            text(&mut lines, format!("{id} · revision {revision}"));
        }
        if report.publisher_revisions.is_empty() {
            text(&mut lines, "No imported publisher snapshots.");
        }
        heading(&mut lines, "Unavailable publishers (not empty catalogs)");
        for (id, reason) in &report.unavailable {
            text(&mut lines, format!("{id}: {reason}"));
        }
        if report.unavailable.is_empty() {
            text(&mut lines, "None reported.");
        }
        if let Some(incomplete) = &report.incomplete {
            text(
                &mut lines,
                format!("Incomplete discovery coverage: {incomplete}"),
            );
        }
        if report.omitted_details > 0 {
            text(
                &mut lines,
                format!(
                    "Bounded report omitted {} provenance details; conflict/blocked flags remain authoritative.",
                    report.omitted_details
                ),
            );
        }
    } else if app.config_selection.is_some() {
        text(
            &mut lines,
            "Local-only selection; no imported discovery evidence. Effective values include defaults; they are not all explicit local overrides.",
        );
    } else {
        text(
            &mut lines,
            "No declarative report. Explicit remote/library/demo routes do not inspect ambient configuration.",
        );
    }
    heading(&mut lines, "Metadata, not activation");
    text(
        &mut lines,
        "This inspector never opens a memory reader. Only Enter in the source picker opens an owner.",
    );
    text(
        &mut lines,
        "This view does not refresh catalogs; use config inspect --refresh for that.",
    );
    lines
}

fn details(app: &App, id: &str, label: &str) -> Vec<Line<'static>> {
    let mut lines = vec![];
    heading(&mut lines, clean(label));
    text(&mut lines, format!("Database ID: {id}"));
    let source = app.displayed_read_source();
    if source
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.owner_db_id == id)
    {
        read_source(&mut lines, &source);
    }
    let browsing = app
        .config_selection
        .as_ref()
        .is_some_and(|snapshot| snapshot.purpose == mneme_config::SelectionPurpose::Browse);
    let selection = app
        .config_selection
        .as_ref()
        .and_then(|snapshot| snapshot.stores.iter().find(|row| row.store.id == id));
    if let Some(row) = selection {
        text(
            &mut lines,
            format!(
                "{}: {}",
                if browsing {
                    "Browse admission"
                } else {
                    "Admission"
                },
                if row.admitted { "admitted" } else { "denied" }
            ),
        );
        if browsing {
            text(
                &mut lines,
                "Automatic activation: not evaluated for browsing",
            );
        } else {
            text(
                &mut lines,
                format!(
                    "Activation: {}",
                    if row.active { "active" } else { "inactive" }
                ),
            );
        }
        if let Some(reason) = &row.reason {
            text(&mut lines, format!("Selection reason: {reason}"));
        }
        if browsing {
            text(
                &mut lines,
                format!("Browse operations: read={} (read-only)", row.eligible.read),
            );
        } else {
            text(
                &mut lines,
                format!(
                    "Eligible operations: read={} write={} core={} context={} recall={} record={} maintenance={}",
                    row.eligible.read,
                    row.eligible.write,
                    row.eligible.core,
                    row.eligible.context,
                    row.eligible.recall,
                    row.eligible.record,
                    row.eligible.maintenance
                ),
            );
        }
    } else {
        if browsing {
            text(&mut lines, "Browse admission: not resolved");
            text(
                &mut lines,
                "Automatic activation: not evaluated for browsing",
            );
        } else {
            text(&mut lines, "Admission: not resolved");
            text(&mut lines, "Activation: not resolved");
        }
    }
    if let Some(report) = &app.config_report {
        let Some(store) = report.stores.get(id) else {
            return lines;
        };
        text(
            &mut lines,
            format!(
                "{}: {}",
                if browsing {
                    "Automatic policy resolution"
                } else {
                    "Resolution"
                },
                if store.blocked { "blocked" } else { "resolved" }
            ),
        );
        if browsing && store.blocked {
            text(
                &mut lines,
                "Policy diagnostics are not an owner connectivity test.",
            );
        }
        for conflict in report
            .conflicts
            .iter()
            .filter(|conflict| conflict.store_id == id)
        {
            heading(&mut lines, format!("CONFLICT · {}", clean(&conflict.field)));
            text(&mut lines, &conflict.reason);
            text(
                &mut lines,
                format!("Publishers: {}", conflict.contributors.join(", ")),
            );
        }
        for (field, resolved) in &store.fields {
            lines.push(Line::default());
            heading(
                &mut lines,
                format!(
                    "{field}{}",
                    if resolved.conflict {
                        " · CONFLICT"
                    } else {
                        ""
                    }
                ),
            );
            text(
                &mut lines,
                format!("Effective: {}", value(resolved.effective.as_ref())),
            );
            text(
                &mut lines,
                format!("Local override: {}", resolved.local_override),
            );
            for (kind, declarations) in [
                ("Contributors", &resolved.contributors),
                ("Shadowed declarations", &resolved.shadowed),
            ] {
                text(&mut lines, format!("{kind}: {}", declarations.len()));
                for contributor in declarations {
                    text(
                        &mut lines,
                        format!("  Publisher: {}", contributor.publisher_db_id),
                    );
                    text(
                        &mut lines,
                        format!("  Value: {}", value(contributor.value.as_ref())),
                    );
                    text(
                        &mut lines,
                        format!("  Branch witness: {}", contributor.witness.join(" → ")),
                    );
                }
            }
        }
        if report.omitted_details > 0 {
            text(
                &mut lines,
                format!(
                    "Report omitted {} details; see Overview.",
                    report.omitted_details
                ),
            );
        }
    } else if let Some(row) = selection {
        heading(
            &mut lines,
            "Effective local configuration (including defaults)",
        );
        if let Ok(json) = serde_json::to_string_pretty(&row.store) {
            for line in json.lines() {
                text(&mut lines, line);
            }
        }
        text(
            &mut lines,
            "No imported provenance. Effective values are not evidence of explicit local presence.",
        );
    }
    lines
}

pub(crate) fn draw(frame: &mut Frame<'_>, screen: Rect, app: &App) {
    let Some(state) = &app.config_inspector else {
        return;
    };
    state.hits.borrow_mut().clear();
    state.viewport.borrow_mut().take();
    if screen.width == 0 || screen.height == 0 {
        return;
    }
    let rows = rows(app);
    let selected = state.selected.min(rows.len());
    let source = app.displayed_read_source();
    let matching_source = source
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| selected == 0 || rows[selected - 1].0 == snapshot.owner_db_id);
    let title = if !matching_source {
        " Configuration "
    } else if source.mixed {
        "SNAPSHOT+ · read-only"
    } else {
        " SNAPSHOT · read-only "
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(MINT))
        .style(Style::default().bg(PANEL).fg(INK));
    let inner = block.inner(screen);
    frame.render_widget(block, screen);
    if inner.width == 0 || inner.height < 2 {
        return;
    }
    let heading = if selected == 0 {
        "Overview".to_owned()
    } else {
        clean(rows[selected - 1].1)
    };
    let wide = inner.width >= 70;
    let browsing = app
        .config_selection
        .as_ref()
        .is_some_and(|snapshot| snapshot.purpose == mneme_config::SelectionPurpose::Browse);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let detail_area = if wide {
        let width = (body.width / 3).min(34);
        let capacity = usize::from(body.height);
        let start = selected.saturating_sub(capacity.saturating_sub(1));
        for (row, index) in (start..=rows.len()).take(capacity).enumerate() {
            let label = if index == 0 {
                "Overview"
            } else {
                rows[index - 1].1
            };
            let blocked = index > 0
                && app
                    .config_report
                    .as_ref()
                    .and_then(|report| report.stores.get(rows[index - 1].0))
                    .is_some_and(|store| store.blocked);
            let label = format!(
                "{} {}{}",
                if index == selected { "›" } else { " " },
                label,
                if blocked && browsing {
                    " [auto-blocked]"
                } else if blocked {
                    " [blocked]"
                } else {
                    ""
                }
            );
            let rect = Rect::new(body.x, body.y + row as u16, width, 1);
            frame.render_widget(
                Paragraph::new(crate::render::short(&label, usize::from(width))).style(
                    Style::default().fg(if index == selected {
                        MINT
                    } else if blocked {
                        GOLD
                    } else {
                        DIM
                    }),
                ),
                rect,
            );
            state.hits.borrow_mut().push((rect, index));
        }
        Rect::new(
            body.x + width + 1,
            body.y,
            body.width.saturating_sub(width + 1),
            body.height,
        )
    } else {
        body
    };
    let lines = if selected == 0 {
        overview(app)
    } else {
        details(app, rows[selected - 1].0, rows[selected - 1].1)
    };
    let heights: Vec<_> = lines
        .iter()
        .map(|line| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(detail_area.width)
        })
        .collect();
    let max = heights
        .iter()
        .sum::<usize>()
        .saturating_sub(usize::from(detail_area.height));
    *state.viewport.borrow_mut() = Some(Viewport {
        area: detail_area,
        max,
    });
    // Skip complete logical lines before using Ratatui's u16 physical offset.
    // A long bounded report stays scrollable even at 24x9; no offset wrap.
    let mut offset = state.scroll.min(max);
    let mut first = 0;
    while first < heights.len() && offset >= heights[first] {
        offset -= heights[first];
        first += 1;
    }
    let paragraph = Paragraph::new(lines.into_iter().skip(first).collect::<Vec<_>>())
        .wrap(Wrap { trim: false });
    frame.render_widget(
        paragraph.scroll((offset.min(usize::from(u16::MAX)) as u16, 0)),
        detail_area,
    );
    let status = format!(
        "{}/{} {} · {}/{}",
        selected + 1,
        rows.len() + 1,
        heading,
        state.scroll.min(max) + 1,
        max + 1
    );
    frame.render_widget(
        Paragraph::new(crate::render::short(&status, usize::from(inner.width)))
            .style(Style::default().fg(DIM)),
        Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
    );
    let keys = if inner.width >= 58 {
        "←→ store · ↑↓ scroll · PgUp/Dn · Esc close · q quit"
    } else {
        "←→ row ↑↓ text Esc q"
    };
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(MINT)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

pub(crate) fn demo_report() -> mneme_config::DiscoveryReport {
    use mneme_config::{
        DiscoveryConflict, DiscoveryContributor, DiscoveryReport, FieldDiscoveryReport,
        StoreDiscoveryReport,
    };
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned();
    let a = "publisher-A".to_owned();
    let b = "publisher-B".to_owned();
    let contributor = |publisher: &str, value: bool, witness: &[&str]| DiscoveryContributor {
        publisher_db_id: publisher.into(),
        value: Some(value.into()),
        witness: witness.iter().map(|id| (*id).to_owned()).collect(),
    };
    DiscoveryReport {
        stores: [(
            id.clone(),
            StoreDiscoveryReport {
                label: "Synthetic project".into(),
                blocked: false,
                fields: [
                    (
                        "permissions.read".into(),
                        FieldDiscoveryReport {
                            effective: Some(false.into()),
                            local_override: false,
                            contributors: vec![
                                contributor(&a, true, &["seed", &a]),
                                contributor(&b, false, &["seed", "relay", &b]),
                            ],
                            shadowed: vec![contributor(
                                "deeper-publisher",
                                true,
                                &["seed", &a, "deeper-publisher"],
                            )],
                            conflict: true,
                        },
                    ),
                    (
                        "active.enabled".into(),
                        FieldDiscoveryReport {
                            effective: Some(true.into()),
                            local_override: true,
                            ..Default::default()
                        },
                    ),
                ]
                .into(),
            },
        )]
        .into(),
        conflicts: vec![DiscoveryConflict {
            store_id: id,
            field: "permissions.read".into(),
            contributors: vec![a.clone(), b.clone()],
            reason: "Independent branch declarations disagree; false wins conservatively.".into(),
        }],
        unavailable: [(
            "unanswered-publisher".into(),
            "Catalog unavailable; this is not an empty catalog.".into(),
        )]
        .into(),
        publisher_revisions: [(a, "revision-A1".into()), (b, "revision-B2".into())].into(),
        omitted_details: 0,
        incomplete: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn snapshot_app() -> App {
        let mut app = App::new(vec![], String::new(), false);
        app.graph = crate::model::demo_graph();
        app.graph.read_source = crate::read_source::example();
        app
    }

    fn lines_text(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), app))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overview_preserves_full_displayed_snapshot_witness() {
        let app = snapshot_app();
        assert_eq!(
            app.graph.read_source.snapshot.as_ref().unwrap().captured_at,
            1_700_000_000
        );
        let overview = lines_text(overview(&app));
        for expected in [
            "Mode: SNAPSHOT · read-only",
            "Canonical owner ID: 01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "Captured UTC: 2023-11-14T22:13:20Z",
            "Age:",
            "Source device: test-device",
            "Generation: generation-a",
            "Snapshot database: replica-a",
            "Resolved snapshot path: /synthetic/replica.mnemed",
        ] {
            assert!(
                overview.contains(expected),
                "missing {expected}: {overview}"
            );
        }
    }

    #[test]
    fn unrelated_store_never_inherits_displayed_snapshot_witness() {
        let mut app = snapshot_app();
        let owner_id = app
            .graph
            .read_source
            .snapshot
            .as_ref()
            .unwrap()
            .owner_db_id
            .clone();
        let matching = lines_text(details(&app, &owner_id, "matching store"));
        assert!(matching.contains("Displayed read source"));
        assert!(matching.contains("Generation: generation-a"));

        let unrelated = lines_text(details(&app, "another-owner", "unrelated store"));
        assert!(!unrelated.contains("Displayed read source"));
        assert!(!unrelated.contains("SNAPSHOT"));
        assert!(!unrelated.contains("generation-a"));

        let mut report = demo_report();
        report.stores = [(
            "another-owner".into(),
            report.stores.into_values().next().unwrap(),
        )]
        .into();
        app.config_report = Some(std::sync::Arc::new(report));
        open(&mut app);
        app.config_inspector.as_mut().unwrap().selected = 1;
        assert!(!render(&app, 24, 9).contains("SNAPSHOT"));
    }

    #[test]
    fn absent_witness_stays_unknown_not_live() {
        let mut app = snapshot_app();
        app.graph.read_source = Default::default();
        app.activity = Some(crate::model::Activity {
            state: "ready".into(),
            feed: true,
            ..Default::default()
        });
        let overview = lines_text(overview(&app));
        assert!(overview.contains("Read source: unknown (no snapshot witness retained)."));
        assert!(!overview.contains("SNAPSHOT"));
        assert!(!overview.contains("LIVE"));
        assert!(!overview.contains("Mode: live"));
    }

    #[test]
    fn mixed_witness_is_only_a_snapshot_component() {
        let mut app = snapshot_app();
        app.graph.read_source.mixed = true;
        let overview = lines_text(overview(&app));
        assert!(overview.contains("Mode: MIXED reads; this SNAPSHOT component is read-only."));
        assert!(overview.contains("This witness does not describe every displayed read."));
        assert!(!overview.contains("Mode: SNAPSHOT · read-only"));
    }

    #[test]
    fn snapshot_warning_survives_narrow_inspector_scrolling() {
        for mixed in [false, true] {
            let mut app = snapshot_app();
            app.graph.read_source.mixed = mixed;
            open(&mut app);
            for scroll in [0, usize::MAX] {
                app.config_inspector.as_mut().unwrap().scroll = scroll;
                let rendered = render(&app, 24, 9);
                let title = rendered.lines().next().unwrap();
                assert!(title.contains("SNAPSHOT"), "{rendered}");
                assert!(title.contains("read-only"), "{rendered}");
            }
        }
    }

    #[test]
    fn capture_age_keeps_future_clock_skew_explicit() {
        let captured_seconds = 1_700_000_000;
        assert_eq!(
            snapshot_age_at(captured_seconds, captured_seconds),
            "0s old"
        );
        assert_eq!(
            snapshot_age_at(captured_seconds, captured_seconds + 1),
            "1s old"
        );
        assert_eq!(
            snapshot_age_at(captured_seconds, captured_seconds + 61),
            "1m 1s old"
        );
        assert_eq!(
            snapshot_age_at(captured_seconds, captured_seconds + 3_660),
            "1h 1m old"
        );
        assert_eq!(
            snapshot_age_at(captured_seconds, captured_seconds + 90_000),
            "1d 1h old"
        );
        assert_eq!(
            snapshot_age_at(captured_seconds + 1, captured_seconds),
            "future by 1s (clock skew)"
        );
    }
}
