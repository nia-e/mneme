//! A read-only, bounded observatory; rendering never waits for network work.
mod clusters;
mod data;
mod detail_scroll;
mod input;
mod inventory_actions;
mod layout;
pub mod model;
mod render;
mod scene_actions;
pub mod scenes;
mod source_picker;
mod touchstone_actions;
pub mod touchstones;
use inventory_actions::*;
use scene_actions::*;
use scenes::{SceneAxis, ScenePage};
use touchstone_actions::*;

#[cfg(not(unix))]
use crossterm::event::EnableMouseCapture;
use crossterm::{
    event::{
        self, DisableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use model::*;
pub use model::{SourceChoice, Target, View};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
type Error = Box<dyn std::error::Error + Send + Sync>;

// Mouse capture can leave reports already queued even after motion reporting is
// disabled. Drain ignored reports cheaply, but bound each turn so repainting,
// worker responses and signals still get a chance under a continuous flood.
const INPUT_EVENTS_PER_TURN: usize = 256;
const INPUT_DRAIN_TIME: Duration = Duration::from_millis(2);
// On Linux, use-dev-tty selects Crossterm's level-triggered source. Its 0.29
// zero-timeout path skips BOTH its parser queue and fd readiness check. A small
// positive timeout reaches that check without changing the aggregate drain
// budget. The source uses the same tty_fd() (tty stdin first) as the Mio source.
#[cfg(target_os = "linux")]
const INPUT_POLL_TIME: Duration = Duration::from_millis(1);
#[cfg(not(target_os = "linux"))]
const INPUT_POLL_TIME: Duration = Duration::ZERO;
#[cfg(unix)]
const BUTTON_MOTION_MOUSE: &str = "\x1b[?1006h\x1b[?1003l\x1b[?1002h";

fn input_ready() -> io::Result<bool> {
    event::poll(INPUT_POLL_TIME)
}

/// One lookahead preserves boundaries after drag and wheel bursts. Wheel counts
/// accumulate without dropping distance; clicks still use freshly drawn hitboxes.
#[derive(Default)]
struct InputPump {
    pending: Option<Event>,
    wheel_reports: usize,
}
impl InputPump {
    fn next(
        &mut self,
        mut read: impl FnMut() -> io::Result<Option<Event>>,
    ) -> io::Result<Option<Event>> {
        let started = Instant::now();
        let mut burst: Option<Event> = None;
        self.wheel_reports = 0;
        for _ in 0..INPUT_EVENTS_PER_TURN {
            let Some(event) = self
                .pending
                .take()
                .map_or_else(&mut read, |event| Ok(Some(event)))?
            else {
                return Ok(burst);
            };
            let wheel = matches!(&event, Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown));
            let compatible = burst
                .as_ref()
                .is_none_or(|previous| match (previous, &event) {
                    (Event::Mouse(a), Event::Mouse(b)) => {
                        a.kind == b.kind
                            && (a.kind == MouseEventKind::Drag(MouseButton::Left)
                                || (a.column == b.column
                                    && a.row == b.row
                                    && a.modifiers == b.modifiers))
                    }
                    _ => false,
                });
            if compatible
                && (wheel
                    || matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Drag(MouseButton::Left)))
            {
                self.wheel_reports += usize::from(wheel);
                burst = Some(event);
            } else if burst.is_some() {
                // Never cross a change of direction/pane, click, key or resize.
                // Even ignored noise ends a consecutive motion run.
                self.pending = Some(event);
                return Ok(burst);
            } else {
                let ignored = match &event {
                    Event::Mouse(mouse) => !matches!(
                        mouse.kind,
                        MouseEventKind::Down(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    ),
                    Event::Key(crossterm::event::KeyEvent {
                        kind: KeyEventKind::Release,
                        ..
                    })
                    | Event::FocusGained
                    | Event::FocusLost
                    | Event::Paste(_) => true,
                    _ => false,
                };
                if !ignored {
                    return Ok(Some(event));
                }
            }
            if started.elapsed() >= INPUT_DRAIN_TIME {
                break;
            }
        }
        Ok(burst)
    }

    fn terminal_input(&mut self) -> io::Result<Option<Event>> {
        self.next(|| {
            if input_ready()? {
                event::read().map(Some)
            } else {
                Ok(None)
            }
        })
    }

    fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

pub struct Options {
    pub sources: Vec<SourceChoice>,
    pub query: String,
    pub demo: bool,
    pub view: View,
    /// Nonfatal source-discovery problems, shown inside the first rendered view.
    pub startup_warnings: Vec<String>,
}

fn apply_startup_warnings(app: &mut App, warnings: Vec<String>) {
    for warning in warnings.into_iter().rev() {
        app.event(warning);
    }
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn preview(width: u16, height: u16) -> io::Result<String> {
    preview_view(width, height, View::Map)
}

pub fn preview_view(width: u16, height: u16, view: View) -> io::Result<String> {
    let mut app = App::new(vec![], "demo".into(), true);
    app.graph = demo_graph();
    app.scenes = scenes::demo_scenes(SceneAxis::Recorded);
    app.scenes_loaded = true;
    app.cabinet.page = touchstones::demo_touchstones();
    app.cabinet.loaded = true;
    app.view = view;
    render::preview(width, height, &app)
}

fn worker(
    target: Target,
) -> (
    mpsc::Sender<Request>,
    mpsc::Receiver<Response>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(2);
    let (reply, answers) = mpsc::channel(4);
    (tx, answers, tokio::spawn(data::serve(target, rx, reply)))
}
fn request(app: &mut App, tx: &mpsc::Sender<Request>, req: Request) -> bool {
    if app.busy {
        return false;
    }
    let fetching = !matches!(req, Request::Poll);
    if tx.try_send(req).is_ok() {
        app.busy = true;
        if fetching {
            app.error = None;
        }
        true
    } else {
        false
    }
}
/// A Focus response can navigate or populate a temporary lens. Keeping that
/// intent explicit prevents a late edge read from replacing the user's field.
enum PendingRead {
    Inventory {
        after: Option<String>,
        pages: usize,
        bytes: usize,
        started: Instant,
        waiting: bool,
    },
    Summaries {
        ids: Vec<String>,
    },
    Query(String),
    Navigate(Visit),
    Refresh,
    EdgeLens(String),
    Scenes {
        axis: SceneAxis,
        cue: String,
    },
    Scene {
        episode_id: String,
        edition_id: String,
    },
    Cabinet {
        generation: u64,
    },
    Annotation {
        generation: u64,
        id: String,
    },
    ExactTarget {
        generation: u64,
        db_id: String,
        id: String,
    },
}

// Commit a history entry only once navigation succeeds. Failed reads keep
// both the current view and its back stack intact.
fn focus_selected(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) -> bool {
    let Some(node) = app.selected_node() else {
        return false;
    };
    let id = node.id.clone();
    if app.demo {
        app.remember(app.visit());
        app.close_edge_lens();
        app.graph = demo_neighborhood(&id);
        app.layout.borrow_mut().reset();
        app.selected = 0;
        app.scroll = 0;
        return true;
    }
    if let Some(tx) = tx {
        if request(app, tx, Request::Focus(id)) {
            *pending = Some(PendingRead::Navigate(app.visit()));
            app.close_edge_lens();
            return true;
        }
    }
    false
}

fn toggle_edge_lens(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    if app.edge_lens.is_some() {
        app.close_edge_lens();
        return;
    }
    let Some(node) = app.selected_node() else {
        return;
    };
    let id = node.id.clone();
    if app.demo {
        app.open_edge_lens(id.clone(), demo_neighborhood(&id), LensState::Ready);
    } else if app.graph.focus.as_deref() == Some(id.as_str()) {
        let graph = neighborhood(&app.graph, &id);
        app.open_edge_lens(id, graph, LensState::Ready);
    } else if let Some(tx) = tx {
        if request(app, tx, Request::Lens(id.clone())) {
            let graph = neighborhood(&app.graph, &id);
            app.open_edge_lens(id.clone(), graph, LensState::Loading);
            *pending = Some(PendingRead::EdgeLens(id));
        }
    }
}

fn query_field(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
    query: String,
) -> bool {
    if request(app, tx, Request::Query(query.clone())) {
        app.close_edge_lens();
        *pending = Some(PendingRead::Query(query));
        true
    } else {
        false
    }
}

fn change_selection(app: &mut App, forward: bool) {
    if app.view == View::Map {
        app.cycle_nodes(forward);
        return;
    }
    app.close_edge_lens();
    if app.view == View::Touchstones {
        cabinet_selection(app, forward);
        return;
    }
    if app.view == View::Scenes {
        let count = app.scenes.items.len();
        if count > 0 {
            app.scene_selected = (app.scene_selected + if forward { 1 } else { count - 1 }) % count;
            app.scene_detail = None;
            app.show_details = false;
            app.scroll = 0;
        }
        return;
    }
    let count = app.graph.nodes.len();
    if count > 0 {
        app.selected = (app.selected + if forward { 1 } else { count - 1 }) % count;
        app.scroll = 0;
    }
}

fn open_source_picker(
    app: &mut App,
    queued_user_key: &mut Option<(event::KeyEvent, Option<String>)>,
) {
    // A suspended map intent must never become a modal Enter after cancellation.
    *queued_user_key = None;
    source_picker::open(app);
}

fn reset_source(app: &mut App, pending: &mut Option<PendingRead>) {
    *pending = None;
    app.close_edge_lens();
    app.target = (app.target + 1) % app.targets.len();
    app.source_menu = None;
    app.source_menu_hits.borrow_mut().clear();
    app.graph = Graph::default();
    app.layout.borrow_mut().reset();
    app.cabinet = TouchstoneCabinet::default();
    app.deferred_view_read = false;
    app.scenes = ScenePage::default();
    app.scene_selected = 0;
    app.scene_detail = None;
    app.scenes_loaded = false;
    app.scene_refreshed = None;
    app.selected = 0;
    app.scroll = 0;
    app.activity = None;
    app.activity_samples.clear();
    app.pulses.clear();
    app.history.clear();
    app.show_details = false;
    app.busy = false;
    app.error = None;
    app.last_refresh = None;
}

fn apply_response(app: &mut App, pending: &mut Option<PendingRead>, response: Response) {
    app.busy = false;
    match response {
        Response::Inventory(page) => apply_inventory(app, pending, page),
        Response::Summaries(page) => apply_summaries(app, pending, page),
        Response::Loaded(graph) => {
            let intent = pending.take();
            if let Some(PendingRead::EdgeLens(anchor)) = intent {
                // Closing the lens, changing selection or switching source does
                // not cancel network I/O. Its eventual reply is safely discarded.
                if let Some(lens) = app.edge_lens.as_mut().filter(|lens| lens.anchor == anchor) {
                    // The fresh explicit read can know lifecycle status before
                    // viewport cards hydrate. Duplicate base IDs own rendering;
                    // update only admitted known status, never overwrite their
                    // body/card cache with partial neighbor placeholders.
                    for fresh in &graph.nodes {
                        if matches!(fresh.status.as_str(), "active" | "archived") {
                            if let Some(base) =
                                app.graph.nodes.iter_mut().find(|node| node.id == fresh.id)
                            {
                                base.status.clone_from(&fresh.status);
                            }
                        }
                    }
                    lens.merge_graph(graph);
                    app.error = None;
                    app.normalize_map_selection();
                }
                return;
            }
            let retained = if matches!(intent, Some(PendingRead::Refresh)) {
                app.selected_node().map(|node| node.id.clone())
            } else {
                None
            };
            match intent {
                Some(PendingRead::Query(query)) => {
                    app.layout.borrow_mut().reset();
                    app.query = query;
                    app.history.clear();
                }
                Some(PendingRead::Navigate(visit)) => {
                    app.remember(visit);
                    app.layout.borrow_mut().reset();
                }
                Some(PendingRead::Refresh) => {}
                // Responses belong to one request on one worker connection.
                // An unsolicited/stale graph must not replace a field.
                None => return,
                Some(PendingRead::EdgeLens(_)) => unreachable!(),
                Some(
                    PendingRead::Inventory { .. }
                    | PendingRead::Summaries { .. }
                    | PendingRead::Scenes { .. }
                    | PendingRead::Scene { .. }
                    | PendingRead::Cabinet { .. }
                    | PendingRead::Annotation { .. }
                    | PendingRead::ExactTarget { .. },
                ) => return,
            }
            app.close_edge_lens();
            app.event(format!(
                "Loaded {} memories · {} links",
                graph.nodes.len(),
                graph.edges.len()
            ));
            let previous = app.selected;
            app.selected = retained
                .as_ref()
                .and_then(|id| graph.nodes.iter().position(|node| &node.id == id))
                .unwrap_or_else(|| {
                    if retained.is_some() {
                        previous.min(graph.nodes.len().saturating_sub(1))
                    } else {
                        0
                    }
                });
            app.graph = graph;
            app.scroll = 0;
            app.last_refresh = Some(Instant::now());
            if app.view == View::Map {
                app.deferred_view_read = false;
            }
            app.error = None;
        }
        Response::Scenes(page) => apply_scene_page(app, pending, page),
        Response::Scene(scene) => apply_scene_detail(app, pending, scene),
        Response::Touchstones(page) => apply_cabinet_page(app, pending, page),
        Response::Touchstone(detail) => apply_annotation(app, pending, detail),
        Response::TouchstoneTarget(target) => apply_exact_target(app, pending, target),
        Response::Canceled => {}
        Response::Activity(activity) => {
            for id in &activity.node_ids {
                if app.scene_nodes().iter().any(|node| &node.id == id) {
                    app.pulses.insert(id.clone(), Instant::now());
                }
            }
            let mut sample = activity.clone();
            sample.node_ids.clear();
            app.activity_samples.push_back(sample);
            if app.activity_samples.len() > 60 {
                app.activity_samples.pop_front();
            }
            app.activity = Some(activity);
        }
        Response::Error(error) => {
            if let Some(state) = app.graph.inventory.as_mut() {
                state.crawling = false;
            }
            let intent = pending.take();
            if app.view == View::Map
                && matches!(
                    intent,
                    Some(
                        PendingRead::Inventory { .. }
                            | PendingRead::Summaries { .. }
                            | PendingRead::Query(_)
                            | PendingRead::Navigate(_)
                            | PendingRead::Refresh
                    )
                )
            {
                // A returned initial Map read already served the deferred view,
                // even when it failed. Going out and back must not auto-retry it.
                app.deferred_view_read = false;
            }
            if intent
                .as_ref()
                .is_some_and(|intent| cabinet_intent_is_stale(app, intent))
            {
                return;
            }
            if let Some(PendingRead::Scene {
                episode_id,
                edition_id,
            }) = &intent
            {
                if app.view != View::Scenes
                    || !app.selected_scene().is_some_and(|selected| {
                        &selected.episode_id == episode_id && &selected.edition_id == edition_id
                    })
                {
                    return;
                }
            }
            if let Some(PendingRead::EdgeLens(anchor)) = intent {
                let Some(lens) = app.edge_lens.as_mut().filter(|lens| lens.anchor == anchor) else {
                    return;
                };
                lens.state = LensState::Unavailable;
                app.event("Edges unavailable · keeping the field");
            } else {
                app.event("Source unavailable · keeping the last view");
            }
            app.error = Some(clean(&error));
            app.activity = None;
        }
    }
}

fn back_key(app: &mut App, key: KeyCode) -> bool {
    let previous_view = app.view;
    if key == KeyCode::Esc && app.show_help {
        app.show_help = false;
    } else if app.view == View::Touchstones {
        return cabinet_back(app);
    } else if key == KeyCode::Esc && app.edge_lens.is_some() {
        app.close_edge_lens();
    } else if key == KeyCode::Esc && app.show_details {
        app.show_details = false;
        if app.view == View::Scenes {
            app.scene_detail = None;
        }
        app.scroll = 0;
    } else if app.view == View::Scenes && !app.busy {
        if app.scene_detail.take().is_some() {
            app.show_details = false;
            app.scroll = 0;
        } else {
            app.view = View::Map;
            app.show_details = false;
            app.scroll = 0;
        }
    } else if !app.busy {
        app.go_back();
    }
    previous_view == View::Scenes && app.view == View::Map && app.last_refresh.is_none()
}

async fn stop(tx: &mpsc::Sender<Request>, task: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(30), async {
        let _ = tx.send(Request::Shutdown).await;
        let _ = (&mut *task).await;
    })
    .await
    .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

pub async fn run(options: Options) -> Result<(), Error> {
    let targets: Vec<_> = options
        .sources
        .iter()
        .filter_map(|source| {
            source.route.clone().map(|mut route| {
                route.name = source.name.clone();
                route
            })
        })
        .collect();
    if !options.demo && options.sources.is_empty() {
        return Err("no memory sources available".into());
    }
    let mut app = App::new(targets, options.query, options.demo);
    app.sources = options.sources;
    app.view = options.view;
    let mut startup_warnings = Some(options.startup_warnings);
    if app.demo {
        app.graph = demo_graph();
        app.scenes = scenes::demo_scenes(app.scene_axis);
        app.scenes_loaded = true;
        app.cabinet.page = touchstones::demo_touchstones();
        app.cabinet.loaded = true;
        app.event("Demo only · synthetic memories, no traffic");
    }
    if !app.demo && app.targets.is_empty() {
        app.error = Some(
            "Known sources have no configured live owner route · Tab shows their metadata".into(),
        );
        source_picker::open(&mut app);
    }
    let mut channel = if app.demo || app.targets.is_empty() {
        None
    } else {
        Some(worker(app.targets[0].clone()))
    };
    let mut pending = None;
    let mut cancel_barrier = false;
    let mut queued_user_key = None;
    if let Some((tx, _, _)) = &channel {
        initial_read(&mut app, tx, &mut pending);
    }
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;
    // This UI selects and pans while the left button is held, never on hover.
    // Crossterm's default capture enables all motion, which can put a click
    // (and even q) behind hundreds of reports from merely moving toward a node.
    // Xterm tracking protocols are mutually exclusive: explicitly select 1002
    // AFTER disabling hover, rather than assuming its earlier enable survived.
    // https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-Mouse-Tracking
    #[cfg(unix)]
    execute!(io::stdout(), crossterm::style::Print(BUTTON_MOTION_MOUSE))?;
    #[cfg(not(unix))]
    execute!(io::stdout(), EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut poll_at = Instant::now() + Duration::from_secs(2);
    let mut input_pump = InputPump::default();
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result: Result<(), Error> = async {
        loop {
            if let Some((_, rx, _)) = &mut channel {
                while let Ok(response) = rx.try_recv() {
                    if matches!(response,Response::Canceled) {
                        cancel_barrier=false;app.busy=false;
                    } else if !cancel_barrier {
                        apply_response(&mut app, &mut pending, response);
                    }
                    poll_at = Instant::now() + Duration::from_secs(2);
                }
            }
            if app.deferred_view_read && !app.busy {
                app.deferred_view_read = false;
                if view_is_unread(&app) {
                    if let Some((tx, _, _)) = &channel { initial_read(&mut app, tx, &mut pending); }
                }
            }
            // Initial owner replies may arrive before the first frame. Show
            // discovery warnings afterward so their status cannot bury an
            // unavailable optional scope before the user has seen it.
            if let Some(warnings) = startup_warnings.take() {
                apply_startup_warnings(&mut app, warnings);
            }
            app.normalize_map_selection();
            terminal.draw(|frame| render::draw(frame, &app))?;
            detail_scroll::clamp(&mut app);
            let queued_event=if !cancel_barrier {
                queued_user_key.take().map(|(key,selected_id)| {
                    if let Some(id)=selected_id {
                        if let Some(index)=app.scene_nodes().iter().position(|node|node.id==id){app.select_node(index);}
                    }
                    Event::Key(key)
                })
            }else{None};
            let input_event = match queued_event {
                Some(event) => Some(event),
                None => input_pump.terminal_input()?,
            };
            let discrete_input = input_event.as_ref().is_some_and(|event| {
                !matches!(event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Drag(MouseButton::Left))
            });
            let mut chosen_source = None;
            if let Some(event) = input_event {
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.layout.borrow_mut().cancel_drag();
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            break;
                        }
                        if app.source_menu.is_some() {
                            chosen_source = source_picker::key(&mut app, key.code);
                        } else if app.editing {
                            match key.code {
                                KeyCode::Esc => app.editing = false,
                                KeyCode::Backspace => {
                                    app.input.pop();
                                }
                                KeyCode::Char(c)
                                    if !c.is_control()
                                        && app.input.len() + c.len_utf8() <= 4096 =>
                                {
                                    app.input.push(c)
                                }
                                KeyCode::Enter if !app.input.trim().is_empty() || app.view == View::Scenes => {
                                    if let Some((tx, _, _)) = &channel {
                                        let q = app.input.clone();
                                        let accepted = if app.view == View::Scenes {
                                            let axis = app.scene_axis;
                                            load_scenes(&mut app, Some(tx), &mut pending, axis, q)
                                        } else {
                                            query_field(&mut app, tx, &mut pending, q)
                                        };
                                        if accepted {
                                            app.editing = false;
                                        }
                                    } else {
                                        if app.view == View::Scenes {
                                            let axis = app.scene_axis;
                                            let cue = app.input.clone();
                                            load_scenes(&mut app, None, &mut pending, axis, cue);
                                        } else {
                                            app.event("Demo map is static · connect a live source to search");
                                        }
                                        app.editing = false;
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            // Keep one latest explicit intent, not one new MCP
                            // session per key. A barrier lets the current bounded
                            // read finish, then reuses its verified connection.
                            let background_key=matches!(key.code,KeyCode::Esc|KeyCode::Enter|KeyCode::Char('/'|'e'|'r'|'s'|'t'|'n'|'m'));
                            if background_key && cancel_barrier {
                                queued_user_key=(key.code!=KeyCode::Esc).then(||(key,if matches!(key.code,KeyCode::Enter|KeyCode::Char('e')) && app.view==View::Map {app.selected_node().map(|node|node.id.clone())}else{None}));
                                continue;
                            }
                            if background_key && pause_background(&mut app,&mut pending) {
                                if let Some((tx,_,_))=&channel {
                                    if tx.try_send(Request::Cancel).is_ok() {
                                        cancel_barrier=true;app.busy=true;
                                        queued_user_key=(key.code!=KeyCode::Esc).then(||(key,if matches!(key.code,KeyCode::Enter|KeyCode::Char('e')) && app.view==View::Map {app.selected_node().map(|node|node.id.clone())}else{None}));
                                        app.event("Background reads paused · next user read queued on the same connection");
                                        continue;
                                    }
                                }
                            }
                            if detail_scroll::key(&mut app, key) || input::map_key(&mut app, key) { continue; }
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Char('?') => app.show_help = !app.show_help,
                                KeyCode::Char('i') if app.view == View::Map => {
                                    app.close_edge_lens();
                                    app.show_details = !app.show_details;
                                    app.scroll = 0;
                                }
                                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('b') => {
                                    if back_key(&mut app, key.code) {
                                        if let Some((tx, _, _)) = &channel {
                                            initial_read(&mut app, tx, &mut pending);
                                        }
                                    }
                                }
                                KeyCode::Char('e') if app.view == View::Map => {
                                    toggle_edge_lens(
                                        &mut app,
                                        channel.as_ref().map(|c| &c.0),
                                        &mut pending,
                                    );
                                }
                                KeyCode::Char('/') if app.view != View::Touchstones => {
                                    app.editing = true;
                                    app.input.clear();
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    change_selection(&mut app, true)
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    change_selection(&mut app, false)
                                }
                                KeyCode::Enter if app.view == View::Map => {
                                    focus_selected(
                                        &mut app,
                                        channel.as_ref().map(|c| &c.0),
                                        &mut pending,
                                    );
                                }
                                KeyCode::Enter | KeyCode::Char('i') if app.view == View::Scenes => {
                                    open_scene(&mut app, channel.as_ref().map(|c| &c.0), &mut pending);
                                }
                                KeyCode::Enter | KeyCode::Char('i') if app.view == View::Touchstones => {
                                    open_annotation(&mut app, channel.as_ref().map(|c| &c.0), &mut pending);
                                }
                                KeyCode::Char('t') => {
                                    toggle_cabinet(&mut app, channel.as_ref().map(|c| &c.0), &mut pending);
                                }
                                KeyCode::Char('n' | 'm') if app.view == View::Map => {
                                    if let Some((tx, _, _)) = &channel { continue_inventory(&mut app, tx, &mut pending); }
                                }
                                KeyCode::Char('n') if app.view == View::Touchstones => {
                                    let after = app.cabinet.page.next.clone();
                                    if after.is_some() { load_cabinet(&mut app, channel.as_ref().map(|c| &c.0), &mut pending, after); }
                                }
                                KeyCode::Left | KeyCode::Char('[') if app.view == View::Touchstones => cabinet_reference(&mut app, false),
                                KeyCode::Right | KeyCode::Char(']') if app.view == View::Touchstones => cabinet_reference(&mut app, true),
                                KeyCode::Char('c') if app.view == View::Touchstones => {
                                    open_exact_target(&mut app, channel.as_ref().map(|c| &c.0), &mut pending);
                                }
                                KeyCode::Char('r') if app.view == View::Touchstones => {
                                    load_cabinet(&mut app, channel.as_ref().map(|c| &c.0), &mut pending, None);
                                }
                                KeyCode::Char('s') => {
                                    switch_view(&mut app, channel.as_ref().map(|c| &c.0), &mut pending);
                                }
                                KeyCode::Char('o') if app.view == View::Scenes => {
                                    let axis = app.scene_axis.other();
                                    let cue = app.scene_query.clone();
                                    load_scenes(&mut app, channel.as_ref().map(|c| &c.0), &mut pending, axis, cue);
                                }
                                KeyCode::Char('r') if app.view == View::Scenes => {
                                    let axis = app.scene_axis;
                                    let cue = app.scene_query.clone();
                                    load_scenes(&mut app, channel.as_ref().map(|c| &c.0), &mut pending, axis, cue);
                                }
                                KeyCode::Char('r') => {
                                    if let Some((tx, _, _)) = &channel {
                                        let req = app
                                            .graph
                                            .focus
                                            .clone()
                                            .map(Request::Focus)
                                            .unwrap_or_else(|| if app.graph.inventory.is_some() || app.query.trim().is_empty() { Request::Inventory { after: None } } else { Request::Query(app.query.clone()) });
                                        if request(&mut app, tx, req) {
                                            app.close_edge_lens();
                                            pending = Some(if matches!(app.graph.focus, None) && (app.graph.inventory.is_some() || app.query.trim().is_empty()) {
                                                PendingRead::Inventory { after:None,pages:0,bytes:0,started:Instant::now(),waiting:true }
                                            } else { PendingRead::Refresh });
                                        }
                                    }
                                }
                                KeyCode::Tab => {
                                    open_source_picker(&mut app, &mut queued_user_key);
                                },
                                _ => {}
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        if app.source_menu.is_some() { chosen_source = source_picker::mouse(&mut app, mouse); }
                        else if !detail_scroll::mouse(&mut app, mouse, input_pump.wheel_reports.max(1)) { input::mouse(&mut app, mouse); }
                    },
                    Event::Resize(_, _) => {
                        let mut layout = app.layout.borrow_mut();
                        layout.cancel_drag();
                        layout.invalidate_hits();
                    }
                    _ => {}
                }
            }
            if let Some(index) = chosen_source.filter(|index| *index != app.target) {
                cancel_barrier = false;
                queued_user_key = None;
                // Discard the old answer channel before admitting a new source.
                // Stop only this viewer connection, never the owning service.
                if let Some((tx, _, mut task)) = channel.take() {
                    tokio::spawn(async move { stop(&tx, &mut task).await; });
                }
                reset_source(&mut app, &mut pending);
                app.target = index;
                let new = worker(app.targets[index].clone());
                initial_read(&mut app, &new.0, &mut pending);
                channel = Some(new);
            }
            // Input has first refusal. Only then start another background page
            // or viewport batch; there is one in-flight operation, never a queue.
            let input_waiting = input_pump.has_pending() || input_ready()?;
            // An ignored-only drain must not starve background work forever.
            // A pending actionable burst gets priority; noise does not create
            // a new request per report or enlarge the one-operation queue.
            let background_allowed = app.source_menu.is_none() && (!input_waiting || !discrete_input);
            if !cancel_barrier && background_allowed {
                if let Some((tx, _, _)) = &channel {
                    advance_inventory(&mut app, tx, &mut pending);
                    hydrate_viewport(&mut app, tx, &mut pending);
                }
            }
            if Instant::now() >= poll_at && !app.busy && !cancel_barrier && background_allowed {
                if let Some((tx, _, _)) = &channel {
                    request(&mut app, tx, Request::Poll);
                }
                poll_at = Instant::now() + Duration::from_secs(2);
            }
            app.pulses
                .retain(|_, at| at.elapsed() < Duration::from_secs(2));
            app.tick = app.tick.wrapping_add(1);
            // Never charge an animation-frame delay for every queued report.
            // Still yield through select so input cannot starve termination.
            let pause = if input_waiting {
                Duration::ZERO
            } else {
                Duration::from_millis(80)
            };
            #[cfg(unix)]
            tokio::select! {
                _ = tokio::time::sleep(pause) => {},
                _ = tokio::signal::ctrl_c() => break,
                _ = terminate.recv() => break,
            }
            #[cfg(not(unix))]
            tokio::select! {
                _ = tokio::time::sleep(pause) => {},
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        Ok(())
    }
    .await;
    // Restore the terminal immediately; network cleanup happens off-screen.
    drop(terminal);
    drop(_restore);
    if let Some((tx, _, mut task)) = channel {
        stop(&tx, &mut task).await;
    }
    result
}

#[cfg(test)]
mod input_runtime_tests;
#[cfg(test)]
mod navigation_tests;
