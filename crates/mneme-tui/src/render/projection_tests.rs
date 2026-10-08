//! Keep glyph overlays and the actual Braille canvas on the same dot grid.

use super::*;
use ratatui::buffer::Buffer;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::{Context, Painter};

fn painter_point(star: Star, width: u16, height: u16) -> (usize, usize) {
    let mut context = Context::new(
        width,
        height,
        [0.0, f64::from(width)],
        [0.0, f64::from(height) * 2.0],
        Marker::Braille,
    );
    Painter::from(&mut context)
        .get_point(star.x, star.y)
        .expect("point must be inside the canvas")
}

#[test]
fn marker_uses_the_canvas_raster_at_odd_even_and_tiny_sizes() {
    for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (7, 5), (74, 15), (80, 29)] {
        // Include corners, exact center, and samples either side of many cell
        // boundaries. The independent oracle is Ratatui's public projection.
        for x_step in 0..=32 {
            for y_step in 0..=32 {
                let star = Star {
                    index: 0,
                    x: f64::from(width) * f64::from(x_step) / 32.0,
                    y: f64::from(height) * 2.0 * f64::from(y_step) / 32.0,
                };
                let (dot_x, dot_y) = painter_point(star, width, height);
                assert_eq!(
                    marker_rect(star, width, height),
                    Rect::new((dot_x / 2) as u16, (dot_y / 4) as u16, 1, 1),
                    "canvas {width}×{height}, point {star:?}",
                );
            }
        }
    }
}

#[test]
fn inverse_cell_centers_land_inside_the_same_actual_canvas_cell() {
    for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (7, 5), (74, 15), (80, 29)] {
        for row in 0..height {
            for column in 0..width {
                let marker = Rect::new(column, row, 1, 1);
                let centered = cell_center(13, marker, width, height);
                let (dot_x, dot_y) = painter_point(centered, width, height);
                assert_eq!(centered.index, 13);
                assert_eq!(
                    (dot_x / 2, dot_y / 4),
                    (usize::from(column), usize::from(row)),
                    "canvas {width}×{height}, marker {marker:?}",
                );
                assert_eq!(marker_rect(centered, width, height), marker);
            }
        }
    }
}

/// Unicode Braille bit order is column-major except for the last row.
fn braille_dots(buffer: &Buffer, colors: &[Color]) -> Vec<(u16, u16)> {
    const BITS: [(u8, u16, u16); 8] = [
        (0, 0, 0),
        (1, 0, 1),
        (2, 0, 2),
        (3, 1, 0),
        (4, 1, 1),
        (5, 1, 2),
        (6, 0, 3),
        (7, 1, 3),
    ];
    let mut dots = Vec::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            let cell = &buffer[(x, y)];
            if !colors.is_empty() && !colors.contains(&cell.fg) {
                continue;
            }
            let Some(codepoint) = cell.symbol().chars().next().map(u32::from) else {
                continue;
            };
            if !(0x2800..=0x28ff).contains(&codepoint) {
                continue;
            }
            let bits = (codepoint - 0x2800) as u8;
            for (bit, dx, dy) in BITS {
                if bits & (1 << bit) != 0 {
                    dots.push((x * 2 + dx, y * 4 + dy));
                }
            }
        }
    }
    dots
}

fn dot_bounds_center(dots: &[(u16, u16)]) -> (f64, f64) {
    assert!(!dots.is_empty(), "expected visible Braille halo dots");
    let left = dots.iter().map(|p| p.0).min().unwrap();
    let right = dots.iter().map(|p| p.0).max().unwrap();
    let top = dots.iter().map(|p| p.1).min().unwrap();
    let bottom = dots.iter().map(|p| p.1).max().unwrap();
    (f64::from(left + right) / 2.0, f64::from(top + bottom) / 2.0)
}

#[test]
fn actual_circle_raster_is_centered_on_the_glyph_cell() {
    for (width, height) in [(20, 10), (21, 11), (74, 15), (80, 29)] {
        for (column, row) in [
            (width / 2, height / 2),
            (width / 3, height / 3),
            (width * 2 / 3, height * 2 / 3),
        ] {
            for radius in [1.8, 3.0] {
                let marker = Rect::new(column, row, 1, 1);
                let star = cell_center(0, marker, width, height);
                let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
                Canvas::default()
                    .marker(Marker::Braille)
                    .x_bounds([0.0, f64::from(width)])
                    .y_bounds([0.0, f64::from(height) * 2.0])
                    .paint(|context| {
                        context.draw(&Circle {
                            x: star.x,
                            y: star.y,
                            radius,
                            color: MINT,
                        });
                    })
                    .render(buffer.area, &mut buffer);
                let center = dot_bounds_center(&braille_dots(&buffer, &[MINT]));
                let expected = (f64::from(column) * 2.0 + 0.5, f64::from(row) * 4.0 + 1.5);
                assert!(
                    (center.0 - expected.0).abs() <= 0.5 && (center.1 - expected.1).abs() <= 0.5,
                    "canvas {width}×{height}, marker {marker:?}, radius {radius}: \
                     rendered dot center {center:?}, glyph center {expected:?}",
                );
            }
        }
    }
}

#[test]
fn production_selected_halo_is_not_a_row_above_its_actual_glyph() {
    let mut app = App::new(vec![], String::new(), true);
    app.graph = crate::model::demo_graph();
    app.graph.nodes.truncate(1);
    app.graph.edges.clear();
    // Exercise the full production renderer without reproducing its split-pane
    // geometry. Find the actual glyph rather than guessing its screen offset.
    for (width, height) in [(120, 36), (121, 37), (76, 30)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let glyphs: Vec<_> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer[(x, y)].symbol() == "◆")
            .collect();
        assert_eq!(glyphs.len(), 1, "one selected graph marker must be visible");
        let (_, row) = glyphs[0];
        let dots = braille_dots(buffer, &[MINT, Color::Rgb(45, 83, 79)]);
        let center = dot_bounds_center(&dots);
        // Numeric labels may cover the rightmost ring cells. They do not cover
        // its top or bottom; vertical centering is the reported regression.
        let expected_y = f64::from(row) * 4.0 + 1.5;
        assert!(
            (center.1 - expected_y).abs() <= 0.5,
            "screen {width}×{height}: halo center y={}, glyph center y={expected_y}",
            center.1,
        );
    }
}

fn render_edge_fixture(
    edges: &[Edge],
    points: &[(&str, u16, u16)],
    selected: Option<&str>,
) -> Buffer {
    let (width, height) = (32, 16);
    let by_id: HashMap<_, _> = points
        .iter()
        .enumerate()
        .map(|(index, &(id, column, row))| {
            (
                id,
                cell_center(index, Rect::new(column, row, 1, 1), width, height),
            )
        })
        .collect();
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, f64::from(width)])
        .y_bounds([0.0, f64::from(height) * 2.0])
        .paint(|context| draw_edges(context, edges, &by_id, selected))
        .render(buffer.area, &mut buffer);
    buffer
}

fn edge(from: &str, to: &str, kind: &str) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        kind: kind.into(),
        weight: 0.5,
    }
}

#[test]
fn all_visible_edges_render_with_selected_incidence_not_missing_endpoints() {
    let points = [("a", 3, 4), ("b", 26, 4), ("c", 3, 11), ("d", 26, 11)];
    let edges = [edge("a", "b", "supports"), edge("c", "d", "associative")];
    let selected = render_edge_fixture(&edges, &points, Some("a"));
    let first = render_edge_fixture(&edges[..1], &points, None);
    let second = render_edge_fixture(&edges[1..], &points, None);
    assert!(!braille_dots(&first, &[]).is_empty());
    assert!(!braille_dots(&second, &[]).is_empty());
    assert_eq!(braille_dots(&selected, &[MINT]), braille_dots(&first, &[]));
    assert_eq!(
        braille_dots(&selected, &[Color::Rgb(94, 79, 121)]),
        braille_dots(&second, &[]),
        "an unrelated on-field edge stays visible, just quieter",
    );
    assert_eq!(
        selected,
        render_edge_fixture(&edges, &points, Some("b")),
        "selecting an incoming endpoint highlights the same link",
    );
    let with_missing = [
        edges[0].clone(),
        edge("a", "missing", "supports"),
        edges[1].clone(),
        edge("absent", "c", "transition"),
    ];
    assert_eq!(
        selected,
        render_edge_fixture(&with_missing, &points, Some("a"))
    );
}

#[test]
fn selected_stroke_wins_crossing_color_regardless_of_edge_order() {
    let points = [
        ("left", 3, 8),
        ("right", 26, 8),
        ("top", 15, 2),
        ("bottom", 15, 13),
    ];
    let edges = [
        edge("left", "right", "supports"),
        edge("top", "bottom", "associative"),
    ];
    for (selected, color) in [("left", MINT), ("top", LILAC)] {
        let forward = render_edge_fixture(&edges, &points, Some(selected));
        let reverse = render_edge_fixture(
            &[edges[1].clone(), edges[0].clone()],
            &points,
            Some(selected),
        );
        assert_eq!(forward, reverse);
        assert_eq!(forward[(15, 8)].fg, color);
        assert!(
            forward[(15, 8)]
                .symbol()
                .starts_with(|ch: char| ('\u{2801}'..='\u{28ff}').contains(&ch))
        );
    }
}

#[test]
fn selection_changes_crossing_colors_but_not_stroke_dot_coordinates() {
    let points = [
        ("left", 3, 8),
        ("right", 26, 8),
        ("top", 15, 2),
        ("bottom", 15, 13),
    ];
    let edges = [
        edge("left", "right", "supports"),
        edge("top", "bottom", "associative"),
    ];
    let expected: std::collections::BTreeSet<_> = edges
        .iter()
        .flat_map(|edge| {
            braille_dots(
                &render_edge_fixture(std::slice::from_ref(edge), &points, None),
                &[],
            )
        })
        .collect();
    for selected in [None, Some("left"), Some("top"), Some("not-on-field")] {
        let buffer = render_edge_fixture(&edges, &points, selected);
        let actual = braille_dots(&buffer, &[])
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actual, expected,
            "selected {selected:?} must not erase crossing dots"
        );
    }
}
