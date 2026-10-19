use super::*;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent};
use std::collections::VecDeque;

fn mouse(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 24,
        row: 12,
        modifiers: KeyModifiers::NONE,
    })
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn motion_backlog_reaches_click_and_quit_without_per_report_delay() {
    let mut pump = InputPump::default();
    let click = mouse(MouseEventKind::Down(MouseButton::Left));
    let quit = key(KeyCode::Char('q'));
    let mut events: VecDeque<_> = std::iter::repeat_n(mouse(MouseEventKind::Moved), 500)
        .chain([
            click.clone(),
            mouse(MouseEventKind::Up(MouseButton::Left)),
            quit.clone(),
        ])
        .collect();
    let mut accepted = Vec::new();
    let mut turns = 0;
    while !events.is_empty() || pump.has_pending() {
        if let Some(event) = pump.next(|| Ok(events.pop_front())).unwrap() {
            accepted.push(event);
        }
        turns += 1;
    }
    assert_eq!(
        accepted,
        vec![click, mouse(MouseEventKind::Up(MouseButton::Left)), quit]
    );
    // These are cheap ignored reports, not 500 animation frames. Avoid an exact
    // turn count: the elapsed-time bound may shorten a turn on a slow runner.
    assert!(turns < 50, "500 reports required {turns} turns");
}

#[test]
fn continuous_noise_is_bounded_before_repaint_and_worker_response_handling() {
    let mut pump = InputPump::default();
    let mut reads = 0;
    assert!(
        pump.next(|| {
            reads += 1;
            Ok(Some(mouse(MouseEventKind::Moved)))
        })
        .unwrap()
        .is_none()
    );
    assert!(reads > 0 && reads <= INPUT_EVENTS_PER_TURN);
}

#[test]
fn ignored_key_releases_and_motion_do_not_reorder_resize_or_discrete_input() {
    let mut pump = InputPump::default();
    let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    let resize = Event::Resize(80, 24);
    let arrow = key(KeyCode::Down);
    let mut events = VecDeque::from([
        Event::Key(release),
        mouse(MouseEventKind::Drag(MouseButton::Right)),
        Event::FocusGained,
        resize.clone(),
        mouse(MouseEventKind::ScrollDown),
        mouse(MouseEventKind::ScrollUp),
        mouse(MouseEventKind::ScrollLeft),
        mouse(MouseEventKind::ScrollRight),
        mouse(MouseEventKind::Down(MouseButton::Right)),
        mouse(MouseEventKind::Down(MouseButton::Middle)),
        Event::Paste("ignored paste".into()),
        arrow.clone(),
    ]);
    for expected in [
        resize,
        mouse(MouseEventKind::ScrollDown),
        mouse(MouseEventKind::ScrollUp),
        arrow,
    ] {
        assert_eq!(
            pump.next(|| Ok(events.pop_front())).unwrap(),
            Some(expected)
        );
    }
    assert!(pump.next(|| Ok(events.pop_front())).unwrap().is_none());
}

#[test]
fn input_errors_propagate_instead_of_spinning() {
    let mut pump = InputPump::default();
    let error = pump
        .next(|| Err(io::Error::other("terminal unavailable")))
        .unwrap_err();
    assert_eq!(error.to_string(), "terminal unavailable");
}

#[cfg(unix)]
#[test]
fn capture_selects_button_motion_and_sgr_without_hover_reporting() {
    let capture = BUTTON_MOTION_MOUSE;
    assert!(capture.contains("\x1b[?1006h"));
    assert!(!capture.contains("\x1b[?1003h"));
    let hover_disabled = capture.rfind("\x1b[?1003l").unwrap();
    let drag_enabled = capture.rfind("\x1b[?1002h").unwrap();
    assert!(drag_enabled > hover_disabled);
    // Mouse tracking may be one exclusive state, not independent flags.
    assert!(capture.ends_with("\x1b[?1002h"));
}

fn drag(column: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column,
        row: 12,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn consecutive_left_drags_coalesce_without_crossing_discrete_boundaries() {
    let mut pump = InputPump::default();
    let down = mouse(MouseEventKind::Down(MouseButton::Left));
    let up = mouse(MouseEventKind::Up(MouseButton::Left));
    let arrow = key(KeyCode::Down);
    let resize = Event::Resize(80, 24);
    let mut events = VecDeque::from([
        down.clone(),
        drag(25),
        drag(26),
        arrow.clone(),
        drag(27),
        drag(28),
        up.clone(),
        resize.clone(),
        down.clone(),
    ]);
    for expected in [down.clone(), drag(26), arrow, drag(28), up, resize, down] {
        assert_eq!(
            pump.next(|| Ok(events.pop_front())).unwrap(),
            Some(expected)
        );
    }
    assert!(events.is_empty() && !pump.has_pending());
}

#[test]
fn lookahead_preserves_following_key_after_terminal_queue_is_empty() {
    let mut pump = InputPump::default();
    let quit = key(KeyCode::Char('q'));
    let mut events = VecDeque::from([drag(25), drag(26), quit.clone()]);
    assert_eq!(
        pump.next(|| Ok(events.pop_front())).unwrap(),
        Some(drag(26))
    );
    assert!(events.is_empty());
    assert!(pump.has_pending());
    assert_eq!(
        pump.next(|| panic!("lookahead must precede another read"))
            .unwrap(),
        Some(quit)
    );
    assert!(!pump.has_pending());
}

#[test]
fn continuous_drag_flood_returns_latest_coordinate_within_bounded_work() {
    let mut pump = InputPump::default();
    let mut reads = 0;
    let accepted = pump
        .next(|| {
            reads += 1;
            Ok(Some(drag(reads)))
        })
        .unwrap();
    assert!(reads > 0 && usize::from(reads) <= INPUT_EVENTS_PER_TURN);
    assert_eq!(accepted, Some(drag(reads)));
}

#[test]
fn finite_drag_flood_reaches_final_coordinate_then_release_resize_and_quit() {
    let mut pump = InputPump::default();
    let up = mouse(MouseEventKind::Up(MouseButton::Left));
    let resize = Event::Resize(80, 24);
    let quit = key(KeyCode::Char('q'));
    let mut events: VecDeque<_> = (1..=500)
        .map(drag)
        .chain([up.clone(), resize.clone(), quit.clone()])
        .collect();
    let mut accepted = Vec::new();
    while !events.is_empty() || pump.has_pending() {
        if let Some(event) = pump.next(|| Ok(events.pop_front())).unwrap() {
            accepted.push(event);
        }
    }
    assert!(accepted.len() < 50);
    assert_eq!(
        &accepted[accepted.len() - 4..],
        &[drag(500), up, resize, quit]
    );
}

#[test]
fn wheel_bursts_accumulate_without_crossing_direction_position_or_key_boundaries() {
    let mut pump = InputPump::default();
    let mut outside = mouse(MouseEventKind::ScrollDown);
    if let Event::Mouse(event) = &mut outside {
        event.column = 3;
    }
    let mut events: VecDeque<_> = std::iter::repeat_n(mouse(MouseEventKind::ScrollDown), 500)
        .chain([
            mouse(MouseEventKind::ScrollUp),
            outside.clone(),
            key(KeyCode::Char('q')),
        ])
        .collect();
    let mut down_reports = 0;
    let mut turns = 0;
    while down_reports < 500 {
        assert_eq!(
            pump.next(|| Ok(events.pop_front())).unwrap(),
            Some(mouse(MouseEventKind::ScrollDown))
        );
        down_reports += pump.wheel_reports;
        turns += 1;
    }
    assert_eq!(down_reports, 500);
    assert!(turns < 50, "wheel burst took {turns} repaint turns");
    assert_eq!(
        pump.next(|| Ok(events.pop_front())).unwrap(),
        Some(mouse(MouseEventKind::ScrollUp))
    );
    assert_eq!(pump.wheel_reports, 1);
    assert_eq!(pump.next(|| Ok(events.pop_front())).unwrap(), Some(outside));
    assert_eq!(pump.wheel_reports, 1);
    assert_eq!(
        pump.next(|| Ok(events.pop_front())).unwrap(),
        Some(key(KeyCode::Char('q')))
    );
    assert_eq!(pump.wheel_reports, 0);
}

#[test]
fn continuous_wheel_flood_yields_with_a_bounded_report_count() {
    let mut pump = InputPump::default();
    let mut reads = 0;
    let result = pump
        .next(|| {
            reads += 1;
            Ok(Some(mouse(MouseEventKind::ScrollDown)))
        })
        .unwrap();
    assert_eq!(result, Some(mouse(MouseEventKind::ScrollDown)));
    assert!(reads > 0 && reads <= INPUT_EVENTS_PER_TURN);
    assert_eq!(pump.wheel_reports, reads);
}

#[test]
fn help_consumes_read_navigation_and_cancel_keys_before_underlying_dispatch() {
    for view in [View::Map, View::Scenes, View::Touchstones] {
        let mut app = App::new(vec![], "original query".into(), false);
        app.view = view;
        app.graph = demo_graph();
        app.graph.inventory = Some(InventoryState {
            crawling: true,
            ..Default::default()
        });
        app.selected = 2;
        app.scroll = 7;
        app.show_details = true;
        app.busy = true;
        let mut pending = Some(PendingRead::Inventory {
            after: None,
            pages: 0,
            bytes: 0,
            started: Instant::now(),
            waiting: true,
        });
        render::help::open(&mut app);
        for code in [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('/'),
            KeyCode::Char('e'),
            KeyCode::Char('r'),
            KeyCode::Char('s'),
            KeyCode::Char('t'),
            KeyCode::Char('n'),
            KeyCode::Char('m'),
            KeyCode::Char('b'),
            KeyCode::Char('i'),
            KeyCode::Char('c'),
            KeyCode::Char('o'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('h'),
        ] {
            // This is the event-loop's first gate. A consumed event cannot reach
            // pause_background, request dispatch, selection or the source picker.
            assert_eq!(
                render::help::input(&mut app, &key(code), 1),
                render::help::Input::Consumed
            );
            assert_eq!(app.view, view);
            assert_eq!(app.selected, 2);
            assert_eq!(app.scroll, 7);
            assert_eq!(app.query, "original query");
            assert!(!app.editing);
            assert!(app.source_menu.is_none());
            assert!(app.show_details && app.busy);
            assert!(app.events.is_empty());
            let inventory = app.graph.inventory.as_ref().unwrap();
            assert!(inventory.crawling && !inventory.background_paused);
        }
        assert_eq!(
            render::help::input(&mut app, &key(KeyCode::Esc), 1),
            render::help::Input::Consumed
        );
        assert!(!app.show_help);
        assert!(app.show_details && app.busy);
        // The unchanged underlying background read remains cancellable after
        // dismissal; help did not cancel it or throw away its completion intent.
        assert!(pause_background(&mut app, &mut pending));
    }
}

#[test]
fn help_defers_queued_read_and_selection_restore_until_dismissal() {
    let mut app = App::new(vec![], String::new(), true);
    app.graph = demo_graph();
    app.selected = 0;
    let id = app.graph.nodes[3].id.clone();
    for code in [KeyCode::Enter, KeyCode::Char('e')] {
        app.selected = 0;
        let mut queued = Some((KeyEvent::new(code, KeyModifiers::NONE), Some(id.clone())));
        render::help::open(&mut app);
        assert!(queued_input(&mut app, true, &mut queued).is_none());
        assert!(queued_input(&mut app, false, &mut queued).is_none());
        assert!(queued.is_some());
        assert_eq!(app.selected, 0);
        render::help::input(&mut app, &key(KeyCode::Esc), 1);
        assert_eq!(queued_input(&mut app, false, &mut queued), Some(key(code)));
        assert!(queued.is_none());
        assert_eq!(app.selected, 3);
    }
}

#[test]
fn modal_help_keeps_quit_and_interrupt_available() {
    let mut app = App::new(vec![], String::new(), true);
    render::help::open(&mut app);
    assert_eq!(
        render::help::input(&mut app, &key(KeyCode::Char('q')), 1),
        render::help::Input::Quit
    );
    let interrupt = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        render::help::input(&mut app, &interrupt, 1),
        render::help::Input::Quit
    );
}

#[test]
fn telemetry_poll_does_not_relabel_failed_or_empty_map_as_loading() {
    let mut app = App::new(vec![], String::new(), false);
    let (tx, mut rx) = mpsc::channel(2);
    app.error = Some("Connection refused".into());
    assert!(request(&mut app, &tx, Request::Poll));
    assert!(matches!(rx.try_recv(), Ok(Request::Poll)));
    let frame = render::preview(100, 30, &app).unwrap();
    assert!(frame.contains("Map unavailable"));
    assert!(!frame.contains("Reading this source"));
    app.busy = false;
    app.error = None;
    app.last_refresh = Some(Instant::now());
    assert!(request(&mut app, &tx, Request::Poll));
    let frame = render::preview(100, 30, &app).unwrap();
    assert!(frame.contains("No memories in this field"));
    assert!(!frame.contains("Reading this source"));
}
