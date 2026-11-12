use crate::{
    model::{App, Input, InputMode, Preferences, demo_inventory},
    platform::{normalize_apps, normalize_inputs},
    state::{Direction, Effect, Overlay, Page, State, Target},
    storage, view,
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

#[path = "recents-tests.rs"]
mod recent_tests;

#[path = "wallpapers-ui-tests.rs"]
mod wallpaper_tests;

#[path = "input-refresh-tests.rs"]
mod input_refresh_tests;

fn ready() -> State {
    let mut state = State::new(Preferences::default());
    state.set_inventory(demo_inventory(), None);
    state
}

#[test]
fn home_relaunch_returns_from_apps_and_settings() {
    for page in [1, 2] {
        let mut state = ready();
        state.open(page);
        state.query = "YouTube".into();
        state.filter = 1;
        state.scroll = 400.;
        state.subpage = Some(crate::state::Subpage::Clock);
        state.pointer = true;
        state.pressed = Some(Target::Nav(page));
        state.dirty = false;

        assert!(state.return_home().is_empty());
        assert_eq!(state.page, Page::Home);
        assert_eq!(state.focus, Target::Nav(0));
        assert!(state.query.is_empty());
        assert_eq!(state.filter, 0);
        assert_eq!(state.scroll, 0.);
        assert!(state.subpage.is_none());
        assert!(!state.pointer);
        assert!(state.pressed.is_none());
        assert!(state.dirty);
        assert!(state.return_home().is_empty());
        assert!(state.overlay.is_none(), "Home again must not open Recents");
    }
}

#[test]
fn home_relaunch_dismisses_overlays_without_cancelling_mutations() {
    for overlay in [
        Overlay::Options {
            id: "test.app".into(),
            restore: Target::Nav(1),
        },
        Overlay::Delete {
            id: "test.app".into(),
            restore: Target::Nav(1),
        },
        Overlay::WallpaperUpload,
        Overlay::BackgroundOptions {
            id: "dunes".into(),
            restore: Target::Sidebar(1),
        },
        Overlay::BackgroundDelete {
            id: "dunes".into(),
            restore: Target::Sidebar(1),
        },
    ] {
        let mut state = ready();
        state.open(2);
        state.overlay = Some(overlay);
        state.panel_started = Some(std::time::Instant::now());
        state.wallpaper_upload = Some(Err("test".into()));
        state.uninstalling = Some("test.app".into());
        state.deleting_wallpaper = Some("dunes".into());
        state.moving = Some("test.app".into());

        assert!(state.return_home().is_empty());
        assert_eq!(state.page, Page::Home);
        assert!(state.overlay.is_none());
        assert!(state.panel_started.is_none());
        assert!(state.wallpaper_upload.is_none());
        assert!(state.moving.is_none());
        assert_eq!(state.uninstalling.as_deref(), Some("test.app"));
        assert_eq!(state.deleting_wallpaper.as_deref(), Some("dunes"));
    }
}

#[test]
fn home_relaunch_commits_keyboard_edits_before_clearing_the_subpage() {
    for system in [false, true] {
        let mut state = ready();
        state.open(2);
        state.subpage = Some(crate::state::Subpage::Input("HDMI_1".into()));
        state.open_keyboard(crate::state::TextField::InputName);
        if system {
            state.overlay = Some(Overlay::SystemKeyboard);
        }
        state.text("Console");

        assert_eq!(state.return_home(), vec![Effect::Save]);
        assert_eq!(
            state
                .preferences
                .input_names
                .get("HDMI_1")
                .map(String::as_str),
            Some("Console")
        );
        assert_eq!(state.page, Page::Home);
        assert_eq!(state.focus, Target::Nav(0));
        assert!(!state.text_input_active());
        assert!(state.subpage.is_none());
    }
}

#[test]
fn home_relaunch_dismisses_recents_and_ignores_its_pending_reply() {
    let mut state = ready();
    state.open(1);
    state.open_recents();
    state.recents.visibility = 1.;
    state.recents.closing_focus = Some(Target::RecentEmpty);
    assert!(state.recents.pending.is_some());
    let generation = state.recents.generation;
    let command = state.recents.pending.clone().unwrap();

    assert!(state.return_home().is_empty());
    assert_eq!(state.page, Page::Home);
    assert!(state.overlay.is_none());
    assert!(state.recents.pending.is_none());
    assert_eq!(state.recents.visibility, 0.);
    assert!(state.recents.closing_focus.is_none());
    assert!(
        state
            .recent_response(generation, command, Ok(Some(vec![])))
            .is_empty()
    );
    assert_eq!(state.focus, Target::Nav(0));
    assert!(state.overlay.is_none());
}

#[test]
fn the_app_copy_returns_to_its_home_page_with_recents_closed_but_home_keeps_its_page() {
    let mut state = ready();
    state.open(1);
    state.open_recents();
    state.recents.visibility = 1.;
    assert!(state.returned_to_foreground(false).is_empty());
    assert_eq!(state.page, Page::Home);
    assert!(state.overlay.is_none());
    assert!(state.recents.pending.is_none());
    assert_eq!(state.recents.visibility, 0.);
    assert_eq!(state.focus, Target::Nav(0));

    let mut home = ready();
    home.open(1);
    assert!(home.returned_to_foreground(true).is_empty());
    assert_eq!(home.page, Page::Apps);
}

fn with_astro_input(state: &mut State) {
    let mut inventory = state.inventory.clone();
    inventory.inputs.push(Input {
        id: crate::platform::APP_ID.into(),
        title: "Astro".into(),
        port: "Astro".into(),
        connected: Some(true),
        signal: None,
    });
    state.set_inventory(inventory, None);
}

#[test]
fn home_switches_to_astros_input_once_per_open_and_never_twice_within_the_interval() {
    use crate::state::{ASTRO_INPUT_INTERVAL, AstroInput};
    use std::time::{Duration, Instant};
    let relaunch = Some(Effect::SelectAstroInput);
    let now = Instant::now();
    let mut state = ready();
    with_astro_input(&mut state);
    state.home_opened(false);
    assert_eq!(state.astro_input_due(now), relaunch);
    assert_eq!(state.astro_input, AstroInput::Idle);
    assert_eq!(state.astro_input_due(now), None);
    // A Home key relaunch within the interval does not switch again.
    state.home_opened(false);
    assert_eq!(state.astro_input_due(now + Duration::from_secs(1)), None);
    assert_eq!(state.astro_input, AstroInput::Idle);
    // Focus coming back (the app copy exited or crashed) never arms it.
    assert!(state.returned_to_foreground(true).is_empty());
    let later = now + ASTRO_INPUT_INTERVAL;
    assert_eq!(state.astro_input_due(later), None);
    state.home_opened(false);
    assert_eq!(state.astro_input_due(later), relaunch);
}

#[test]
fn a_home_start_switches_once_its_window_has_focus_and_leaves_the_relaunch_limits_alone() {
    use crate::state::AstroInput;
    use std::time::Duration;
    let start = Some(Effect::SelectAstroInput);
    let mut state = ready();
    // A start waits for the first inventory that lists Astro's input...
    state.home_opened(true);
    let opened = state.home_started_at.expect("start time");
    assert_eq!(state.astro_input_due(opened), None);
    with_astro_input(&mut state);
    assert_eq!(state.astro_input, AstroInput::Started);
    // ...and for Home's window to gain focus. The Home key from an HDMI input starts Home, whose
    // window gains focus a moment after its inventory (webOS 25 log: the system still reported
    // com.webos.app.hdmi2 in front at the inventory, then "Astro window: FocusGained").
    let inventory = opened + Duration::from_millis(500);
    assert_eq!(state.astro_input_due(inventory), None);
    assert_eq!(state.astro_input, AstroInput::Started);
    let focused = opened + Duration::from_millis(900);
    state.home_focused(focused);
    assert_eq!(state.astro_input_due(focused), start);
    assert_eq!(state.astro_input_due(focused), None);
    assert_eq!(state.astro_input_switched, None);
    assert_eq!(state.astro_input_streak, 0);
    // The Home key right after a start still switches.
    state.home_key();
    assert_eq!(state.astro_input_due(focused), start);
    // Hidden after its inventory but before the switch: a start behind another app stays.
    let mut hidden = ready();
    hidden.home_opened(true);
    with_astro_input(&mut hidden);
    hidden.home_hidden();
    hidden.home_focused(hidden.home_started_at.unwrap());
    assert_eq!(
        hidden.astro_input_due(hidden.home_started_at.unwrap()),
        None
    );
}

#[test]
fn a_home_start_switches_on_focus_before_or_after_its_inventory_but_not_on_a_late_focus() {
    use crate::state::{ASTRO_INPUT_START_WINDOW, AstroInput};
    use std::time::Duration;
    // Focus before the inventory: the switch waits for the inventory, even a slow one.
    let mut early = ready();
    early.home_opened(true);
    let opened = early.home_started_at.unwrap();
    early.home_focused(opened + Duration::from_millis(300));
    let slow_inventory = opened + ASTRO_INPUT_START_WINDOW + Duration::from_secs(5);
    assert_eq!(early.astro_input_due(slow_inventory), None);
    with_astro_input(&mut early);
    assert_eq!(
        early.astro_input_due(slow_inventory),
        Some(Effect::SelectAstroInput)
    );
    // Home started behind another app (after a power-on into Astro's input) gains focus only when
    // the user leaves that app: that must not send them back to it.
    let mut behind = ready();
    behind.home_opened(true);
    let opened = behind.home_started_at.unwrap();
    with_astro_input(&mut behind);
    let late = opened + ASTRO_INPUT_START_WINDOW + Duration::from_secs(1);
    behind.home_focused(late);
    assert_eq!(behind.astro_input_due(late), None);
    assert_eq!(behind.astro_input, AstroInput::Idle);
    // Without any focus, the start expires after the window and never fires afterwards.
    let mut unfocused = ready();
    unfocused.home_opened(true);
    let opened = unfocused.home_started_at.unwrap();
    with_astro_input(&mut unfocused);
    assert_eq!(
        unfocused.astro_input_due(opened + Duration::from_secs(1)),
        None
    );
    assert_eq!(unfocused.astro_input, AstroInput::Started);
    let expired = opened + ASTRO_INPUT_START_WINDOW + Duration::from_millis(1);
    assert_eq!(unfocused.astro_input_due(expired), None);
    assert_eq!(unfocused.astro_input, AstroInput::Idle);
    unfocused.home_focused(expired);
    assert_eq!(unfocused.astro_input_due(expired), None);
    // Focus exactly at the window edge still counts; without focus the start keeps waiting there.
    let mut edge = ready();
    edge.home_opened(true);
    let opened = edge.home_started_at.unwrap();
    with_astro_input(&mut edge);
    let at_edge = opened + ASTRO_INPUT_START_WINDOW;
    assert_eq!(edge.astro_input_due(at_edge), None);
    assert_eq!(edge.astro_input, AstroInput::Started);
    edge.home_focused(at_edge);
    assert_eq!(
        edge.astro_input_due(at_edge),
        Some(Effect::SelectAstroInput)
    );
    // The Home key after an expired start still switches, as a relaunch.
    unfocused.home_key();
    assert_eq!(
        unfocused.astro_input_due(expired),
        Some(Effect::SelectAstroInput)
    );
    // Focus gained and lost before the inventory (the user went to another input): the late
    // inventory does not open Astro over it, but a new focus within the window does.
    let mut left = ready();
    left.home_opened(true);
    let opened = left.home_started_at.unwrap();
    left.home_focused(opened + Duration::from_millis(400));
    left.home_focus_lost();
    with_astro_input(&mut left);
    assert_eq!(left.astro_input_due(opened + Duration::from_secs(2)), None);
    assert_eq!(left.astro_input, AstroInput::Started);
    left.home_focused(opened + Duration::from_secs(3));
    assert_eq!(
        left.astro_input_due(opened + Duration::from_secs(3)),
        Some(Effect::SelectAstroInput)
    );
    // Only the first focus of a start counts: a later one cannot move it into the window.
    let mut refocused = ready();
    refocused.home_opened(true);
    let opened = refocused.home_started_at.unwrap();
    refocused.home_focused(opened + ASTRO_INPUT_START_WINDOW + Duration::from_secs(2));
    refocused.home_focused(opened + Duration::from_secs(1));
    with_astro_input(&mut refocused);
    assert_eq!(
        refocused.astro_input_due(opened + Duration::from_secs(1)),
        None
    );
}

#[test]
fn home_stops_after_three_quick_switches_until_the_home_key_and_settles_after_a_pause() {
    use crate::state::{ASTRO_INPUT_INTERVAL, ASTRO_INPUT_LIMIT, ASTRO_INPUT_SETTLE};
    let relaunch = Some(Effect::SelectAstroInput);
    let mut now = std::time::Instant::now();
    let mut state = ready();
    with_astro_input(&mut state);
    for _ in 0..ASTRO_INPUT_LIMIT {
        state.home_opened(false);
        assert_eq!(state.astro_input_due(now), relaunch);
        now += ASTRO_INPUT_INTERVAL;
    }
    // The input copy keeps exiting and SAM keeps relaunching Home: stop.
    state.home_opened(false);
    assert_eq!(state.astro_input_due(now), None);
    // The Home key in Home is user input and starts a new streak.
    state.home_key();
    assert_eq!(state.astro_input_due(now), relaunch);
    // After a quiet pause, Home switches again without the key.
    now += ASTRO_INPUT_SETTLE;
    state.home_opened(false);
    assert_eq!(state.astro_input_due(now), relaunch);
    assert_eq!(state.astro_input_streak, 1);
}

#[test]
fn a_home_start_waits_past_a_failed_input_read_and_a_hidden_start_never_switches() {
    use crate::state::AstroInput;
    let now = std::time::Instant::now();
    let mut state = ready();
    let inventory = state.inventory.clone();
    state.home_opened(true);
    state.set_inventory(inventory, Some("inputs unavailable".into()));
    assert_eq!(state.astro_input, AstroInput::AfterInventory);
    with_astro_input(&mut state);
    let focused = state.home_started_at.unwrap();
    state.home_focused(focused);
    assert_eq!(
        state.astro_input_due(focused),
        Some(Effect::SelectAstroInput)
    );

    let mut hidden = ready();
    hidden.home_opened(true);
    hidden.home_hidden();
    with_astro_input(&mut hidden);
    assert_eq!(hidden.astro_input_due(now), None);
    // Hiding does not cancel a relaunch switch that is already due.
    hidden.home_opened(false);
    hidden.home_hidden();
    assert_eq!(hidden.astro_input_due(now), Some(Effect::SelectAstroInput));
}

#[test]
fn home_stays_when_astros_input_is_not_registered() {
    let mut state = ready();
    assert!(
        !state
            .inventory
            .inputs
            .iter()
            .any(|input| input.id == crate::platform::APP_ID)
    );
    state.home_opened(false);
    assert_eq!(state.astro_input_due(std::time::Instant::now()), None);
    assert_eq!(state.astro_input, crate::state::AstroInput::Idle);
    let inventory = state.inventory.clone();
    state.home_opened(true);
    state.set_inventory(inventory, None);
    assert_eq!(state.astro_input_due(std::time::Instant::now()), None);
}

#[test]
fn preload_launch_metadata_is_recognized_and_ordinary_launches_are_not() {
    use crate::platform::preloaded;
    // As sam started Home in the background on a webOS 25 TV.
    assert!(preloaded(&json!({
        "@system_native_app": true,
        "preload": "full",
        "nid": "com.webos.app.home"
    })));
    assert!(preloaded(&json!({"preload": "partial"})));
    assert!(preloaded(&json!({"preload": true})));
    assert!(!preloaded(
        &json!({"@system_native_app": true, "nid": "com.danielcanoh22.astro"})
    ));
    assert!(!preloaded(&json!({"preload": ""})));
    assert!(!preloaded(&json!({"preload": false})));
}

#[test]
fn only_astros_own_launch_carries_the_home_parameter_and_both_metadata_shapes_are_read() {
    use crate::platform::{LAUNCHED_BY_HOME, launch_payload, launched_by_home};
    assert_eq!(
        launch_payload("com.danielcanoh22.astro"),
        json!({"id": "com.danielcanoh22.astro", "params": {LAUNCHED_BY_HOME: true}})
    );
    assert_eq!(
        launch_payload("com.webos.app.hdmi3"),
        json!({"id": "com.webos.app.hdmi3"})
    );
    assert!(launched_by_home(
        &json!({"nid": "com.danielcanoh22.astro", LAUNCHED_BY_HOME: true})
    ));
    assert!(launched_by_home(
        &json!({"params": {LAUNCHED_BY_HOME: true}})
    ));
    assert!(!launched_by_home(
        &json!({"nid": "com.danielcanoh22.astro"})
    ));
    assert!(!launched_by_home(&json!({LAUNCHED_BY_HOME: "true"})));
}

fn scene(state: &State) -> view::Scene {
    let assets = view::Assets {
        root: PathBuf::new(),
        version: "test".into(),
        metrics: BTreeMap::new(),
        artwork: crate::app_artwork::Service::new(false),
    };
    let navigation = view::navigation(&assets, state.preferences.language);
    view::scene_with_pill(
        state,
        &assets,
        navigation.items[state.page_index()].x,
        navigation.items[state.page_index()].w,
        time::OffsetDateTime::UNIX_EPOCH,
    )
}

fn controls(state: &State) -> Vec<crate::state::Control> {
    scene(state).controls
}

#[test]
fn favorites_edge_fragments_share_draw_hit_and_wheel_viewports() {
    let mut state = ready();
    let favorites: Vec<_> = state
        .inventory
        .apps
        .iter()
        .take(9)
        .map(|app| app.id.clone())
        .collect();
    state.preferences.favorites = Some(favorites.clone());
    assert_eq!(
        state.hit_test(&controls(&state), 1860., 600.),
        Some(Target::App(favorites[6].clone()))
    );
    assert!(state.wheel_scroll(&controls(&state), Some((1860., 600.)), 0, -1));
    assert!((state.home_scroll - crate::state::HOME_TILE_PITCH).abs() < 0.01);
    assert_eq!(state.scroll, 0.);
    assert_eq!(
        state.hit_test(&controls(&state), 40., 600.),
        Some(Target::App(favorites[0].clone()))
    );
    let scroll = state.home_scroll;
    state.pointer_focus(Target::App(favorites[0].clone()));
    assert_eq!(state.home_scroll, scroll);
    assert!(state.hit_test(&controls(&state), 4., 600.).is_none());
    assert!(state.hit_test(&controls(&state), 1915., 600.).is_none());
    assert!(state.hit_test(&controls(&state), 40., 1060.).is_none());
    let scene = scene(&state);
    let viewport = state.favorites_viewport();
    assert!(scene.draw.iter().any(|draw| matches!(draw, crate::renderer::Draw::Clip(Some(rect)) if rect.x == viewport.x && rect.w == viewport.w)));
    assert!(state.wheel_scroll(&scene.controls, Some((40., 600.)), 0, 1));
    assert_eq!(state.home_scroll, 0.);
    assert_eq!(state.scroll, 0.);
}

#[test]
fn widening_favorites_does_not_widen_apps_or_unclip_offscreen_rows() {
    let mut state = ready();
    state.open(1);
    let control = crate::state::Control {
        target: Target::App("sample".into()),
        rect: crate::state::Rect::new(0., 500., 1920., 600.),
        scrollable: true,
    };
    let controls = [control];
    assert!(state.hit_test(&controls, 40., 600.).is_none());
    assert!(state.hit_test(&controls, 1860., 600.).is_none());
    assert_eq!(
        state.hit_test(&controls, 100., 1000.),
        Some(Target::App("sample".into()))
    );
    assert!(state.hit_test(&controls, 100., 1080.).is_none());
    assert_eq!(
        state.hit_test(&controls, 100., 600.),
        Some(Target::App("sample".into()))
    );
    assert!(!state.wheel_scroll(&controls, Some((1860., 600.)), 0, -1));
    assert_eq!(state.home_scroll, 0.);
    let viewport = state.content_viewport();
    let scroll_limit = controls[0].rect.y + controls[0].rect.h - (viewport.y + viewport.h - 12.);
    assert_eq!(state.scroll, 90_f32.min(scroll_limit));
}

#[test]
fn remote_routes_from_header_and_back_through_settings() {
    let mut state = ready();
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::App("netflix".into()));
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.focus, Target::Nav(0));
    state.open(2);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(0));
    state.activate();
    assert_eq!(state.focus, Target::Setting(0));
    assert!(state.back().is_empty());
    assert_eq!(state.focus, Target::Sidebar(0));
    state.back();
    assert_eq!(state.focus, Target::Nav(2));
    state.back();
    assert_eq!(state.page, Page::Home);
    assert_eq!(
        state.back(),
        vec![Effect::Recent {
            generation: 1,
            command: crate::recents::Command::List
        }]
    );
    assert!(state.recents_open());
}

#[test]
fn hiding_restores_nearest_app_and_hidden_filter_can_restore_it() {
    let mut state = ready();
    state.open(1);
    state.focus = Target::App("netflix".into());
    state.options();
    state.focus = Target::Option(1);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.hidden.contains(&"netflix".into()));
    assert_eq!(state.focus, Target::App("youtube.leanback.v4".into()));
    state.filter = 5;
    assert_eq!(state.apps().len(), 1);
    state.focus = Target::App("netflix".into());
    state.options();
    state.focus = Target::Option(1);
    state.activate();
    assert!(state.preferences.hidden.is_empty());
    assert_eq!(state.focus, Target::App("netflix".into()));
    assert_eq!(state.filter, 0);
}

#[test]
fn reorder_persists_order_and_keeps_selected_identity() {
    let mut state = ready();
    state.focus = Target::App("netflix".into());
    state.hold();
    assert_eq!(
        state.direction(Direction::Right, &controls(&state)),
        vec![Effect::Save]
    );
    assert_eq!(
        &state.preferences.favorites(&state.inventory.apps)[..2],
        &["youtube.leanback.v4", "netflix"]
    );
    assert_eq!(state.focus, Target::App("netflix".into()));
    assert!(state.back().is_empty());
    assert!(state.moving.is_none());
}

#[test]
fn keyboard_is_modal_and_back_preserves_then_clears_search() {
    let mut state = ready();
    state.open(1);
    state.focus = Target::Search;
    state.activate();
    assert!(
        controls(&state)
            .iter()
            .all(|c| matches!(c.target, Target::Key(_) | Target::CloseDialog))
    );
    state.text("netflix");
    state.delete();
    state.text("x");
    state.back();
    assert_eq!(state.query, "netflix");
    assert_eq!(state.focus, Target::Search);
    assert_eq!(state.apps().len(), 1);
    state.back();
    assert!(state.query.is_empty());
    assert_eq!(state.focus, Target::Nav(1));
    assert_eq!(state.page, Page::Apps);
}

#[test]
fn partial_refresh_preserves_valid_inputs_and_never_invents_tv_apps() {
    let mut state = ready();
    state.set_inventory(
        crate::model::Inventory::default(),
        Some("Inputs unavailable".into()),
    );
    assert!(state.inventory.apps.is_empty());
    assert_eq!(state.inventory.inputs.len(), 3);
    assert_eq!(state.first_content(), Target::Browse);
}

#[test]
fn inventory_filters_system_apps_duplicates_and_unsafe_identifiers() {
    let apps = normalize_apps(&json!([
        {"id":"netflix","title":" Netflix\n"}, {"id":"netflix","title":"Duplicate"},
        {"id":"bad;command","title":"Bad"}, {"id":"com.webos.app.hidden","title":"Internal"},
        {"id":"com.webos.app.livetv","visible":false,"title":"Live TV"},
        {"id":"com.ecklf.luma","visible":true},
        {"id":"com.luma.launcher.native","visible":true},
        {"id":"com.webos.app.hdmi1","visible":true},
        {"id":"hidden","visible":"false"}
    ]))
    .unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(
        apps.iter().find(|a| a.id == "netflix").unwrap().title,
        "Netflix"
    );
    assert!(normalize_apps(&json!({"apps":[]})).is_err());
    let inputs = normalize_inputs(&json!([
        {"appId":"com.webos.app.hdmi1","id":"HDMI_1","port":1,"connected":true},
        {"appId":"com.webos.app.hdmi2","id":"HDMI_4"},
        {"appId":"com.webos.app.hdmi3"}, {"appId":"com.webos.app.hdmi3"},
        {"appId":"com.webos.app.hdmi4","label":"Console"}
    ]))
    .unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1].connected, None);
}

#[test]
fn inventory_visibility_cases_match_the_launcher_filters() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../tests/fixtures/app-visibility.json")).unwrap();
    let inventory: Vec<_> = cases.iter().map(|case| case["app"].clone()).collect();
    let actual: std::collections::BTreeSet<_> = normalize_apps(&json!(inventory))
        .unwrap()
        .into_iter()
        .map(|app| app.id)
        .collect();
    let expected: std::collections::BTreeSet<_> = cases
        .iter()
        .filter(|case| case["include"] == true)
        .map(|case| case["app"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn asset_loading_and_unknown_app_cards_do_not_require_collected_artwork() {
    let root = std::env::temp_dir().join(format!("astro-assets-no-dumps-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("version.txt"), "test").unwrap();
    std::fs::write(root.join("font-metrics.json"), "{}").unwrap();
    for webos in [false, true] {
        let assets = view::Assets::load(root.clone(), webos).unwrap();
        let mut state = State::new(Preferences::default());
        state.set_inventory(
            crate::model::Inventory {
                apps: vec![App::new("com.example.regional", "Regional App")],
                inputs: vec![],
            },
            None,
        );
        state.open(1);
        let scene = view::scene_with_pill(
            &state,
            &assets,
            800.,
            100.,
            time::OffsetDateTime::UNIX_EPOCH,
        );
        assert!(scene.draw.iter().any(|draw| matches!(draw,
            crate::renderer::Draw::TintedImage(path, ..) if path.ends_with("ui-icons/generic.lvec"))));
        assert!(scene.draw.iter().any(|draw| matches!(draw,
            crate::renderer::Draw::Text(value, ..) if value == "Regional App")));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn auto_input_visibility_requires_a_positive_connection_report() {
    let mut preferences = Preferences::default();
    let mut input = Input {
        id: "com.webos.app.hdmi1".into(),
        title: "Console".into(),
        port: "HDMI 1".into(),
        connected: None,
        signal: None,
    };
    assert!(!preferences.input_visible(&input));
    input.connected = Some(true);
    assert!(preferences.input_visible(&input));
    preferences
        .input_modes
        .insert(input.id.clone(), InputMode::Off);
    assert!(!preferences.input_visible(&input));
    preferences
        .input_modes
        .insert(input.id.clone(), InputMode::On);
    input.connected = Some(false);
    assert!(preferences.input_visible(&input));
}

#[test]
fn a_save_by_the_other_astro_copy_is_reloaded_once_and_a_broken_file_keeps_preferences() {
    let directory = std::env::temp_dir().join(format!("astro-reload-test-{}", std::process::id()));
    let path = directory.join("preferences.json");
    let mut state = ready();
    storage::save(&path, &state.preferences).unwrap();
    let mut stamp = storage::stamp(&path);
    assert!(stamp.is_some());
    // Nothing changed: the current preferences stay as they are.
    state.preferences.background = "unsaved".into();
    crate::reload_changed_preferences(&path, &mut stamp, &mut state);
    assert_eq!(state.preferences.background, "unsaved");
    // A different size, so the change shows even within one file-time tick.
    let other = Preferences {
        background: "midnight".into(),
        show_home_names: !state.preferences.show_home_names,
        ..state.preferences.clone()
    };
    assert_ne!(other.background.len(), state.preferences.background.len());
    storage::save(&path, &other).unwrap();
    crate::reload_changed_preferences(&path, &mut stamp, &mut state);
    assert_eq!(state.preferences.background, "midnight");
    assert_eq!(state.preferences.show_home_names, other.show_home_names);
    assert_eq!(stamp, storage::stamp(&path));
    std::fs::write(&path, "broken file").unwrap();
    crate::reload_changed_preferences(&path, &mut stamp, &mut state);
    assert_eq!(state.preferences.background, "midnight");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preferences_round_trip_and_corrupt_data_is_not_silently_overwritten() {
    let directory = std::env::temp_dir().join(format!("astro-native-test-{}", std::process::id()));
    let path = directory.join("preferences.json");
    let mut preferences = Preferences {
        favorites: Some(vec![]),
        background: "forest".into(),
        ..Preferences::default()
    };
    preferences
        .input_modes
        .insert("com.webos.app.hdmi1".into(), InputMode::Off);
    storage::save(&path, &preferences).unwrap();
    let loaded = storage::load(&path).unwrap();
    assert_eq!(loaded.favorites, Some(vec![]));
    assert_eq!(loaded.background, "forest");
    assert_eq!(loaded.input_modes["com.webos.app.hdmi1"], InputMode::Off);
    std::fs::write(&path, "broken").unwrap();
    assert!(storage::load(&path).is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_old_weather_cache_is_deleted_without_touching_other_files_or_link_targets() {
    let directory =
        std::env::temp_dir().join(format!("astro-weather-cache-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let preferences = directory.join("preferences.json");
    std::fs::write(&preferences, b"{}").unwrap();
    let cache = directory.join("preferences.weather.json");
    std::fs::write(&cache, br#"{"temperature":20}"#).unwrap();
    let neighbour = directory.join("weather.json");
    std::fs::write(&neighbour, b"kept").unwrap();
    storage::remove_weather_cache(&preferences);
    assert!(!cache.exists(), "the cache file is deleted");
    assert!(preferences.exists() && neighbour.exists());
    // Nothing to delete is not an error.
    storage::remove_weather_cache(&preferences);
    #[cfg(unix)]
    {
        let outside = directory.join("outside.json");
        std::fs::write(&outside, b"target").unwrap();
        std::os::unix::fs::symlink(&outside, &cache).unwrap();
        storage::remove_weather_cache(&preferences);
        assert!(
            std::fs::symlink_metadata(&cache).is_err(),
            "the link is removed"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"target");
    }
    // A directory in its place is left alone.
    std::fs::create_dir(&cache).unwrap();
    storage::remove_weather_cache(&preferences);
    assert!(cache.is_dir());
    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn preferences_from_versions_with_weather_and_shortcut_tiles_keep_every_other_setting() {
    let directory =
        std::env::temp_dir().join(format!("astro-removed-fields-test-{}", std::process::id()));
    let path = directory.join("preferences.json");
    std::fs::create_dir_all(&directory).unwrap();
    // Field names and shapes as Astro 3.0.4 saved them.
    let old = json!({
        "favorites": ["netflix", "custom.lamp", "youtube.leanback.v4"],
        "custom_tiles": [{
            "id": "custom.lamp",
            "title": "Lamp",
            "url": "http://homeassistant.local:8123/api/webhook/lamp",
            "method": "POST",
            "icon": "power"
        }],
        "hidden": ["plex"],
        "background": "forest",
        "splash_background": "summit",
        "deleted_backgrounds": ["slate"],
        "show_home_names": true,
        "show_app_names": false,
        "hour_24": true,
        "reduced_motion": true,
        "show_seconds": true,
        "greeting_name": "Ana",
        "show_greeting": false,
        "show_date": false,
        "date_format": "year_month_day",
        "input_modes": {"com.webos.app.hdmi1": "Off"},
        "input_names": {"com.webos.app.hdmi1": "Console"},
        "show_weather": false,
        "weather_location": {
            "name": "Vienna",
            "region": "Vienna",
            "country": "Austria",
            "latitude": 48.2085,
            "longitude": 16.3721
        },
        "temperature_unit": "fahrenheit",
        "language": "es",
        "text_rendering": "analytic",
        "wallpaper_cycle": true,
        "wallpaper_gradient": false,
        "wallpaper_changed": 1700000000
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();
    let loaded = storage::load(&path).unwrap();
    let saved = serde_json::to_value(&loaded).unwrap();
    let mut kept = old.as_object().unwrap().clone();
    for removed in [
        "custom_tiles",
        "show_weather",
        "weather_location",
        "temperature_unit",
    ] {
        kept.remove(removed);
    }
    assert_eq!(saved, serde_json::Value::Object(kept));
    // Saving again drops the removed fields for good.
    storage::save(&path, &loaded).unwrap();
    let rewritten: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(rewritten, saved);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn gallery_and_grid_navigation_scroll_to_keep_focus_visible() {
    let mut state = ready();
    state.set_wallpapers(vec![crate::wallpapers::Wallpaper {
        id: "custom-00000000000000000000000000000001".into(),
        directory: PathBuf::from("/preview/wallpapers/custom-00000000000000000000000000000001"),
        has_original: true,
    }]);
    state.open(2);
    state.section = 1;
    state.focus = Target::Background(0);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Background(3));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Background(6));
    assert_eq!(state.scroll, 0., "the first three gallery rows fit");
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Background(9));
    assert!(state.scroll > 0., "the fourth gallery row must scroll");
    let control = controls(&state)
        .into_iter()
        .find(|c| c.target == state.focus)
        .unwrap();
    let viewport = state.backgrounds_viewport();
    assert!(control.rect.y >= viewport.y);
    assert!(control.rect.y + control.rect.h <= viewport.y + viewport.h);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(1));
    state.open(1);
    let last = state.apps().last().unwrap().id.clone();
    state.focus(Target::App(last), &controls(&state));
    assert!(
        state.scroll > 0.,
        "library focus includes its wrapped caption"
    );
}

#[test]
fn cancelling_options_preserves_identity_and_does_not_save() {
    let mut state = ready();
    state.focus = Target::App("netflix".into());
    state.options();
    assert!(matches!(state.overlay, Some(Overlay::Options { .. })));
    state.focus = Target::CloseDialog;
    assert!(state.activate().is_empty());
    assert_eq!(state.focus, Target::App("netflix".into()));
    assert!(state.overlay.is_none());
}

#[test]
fn pointer_focus_does_not_scroll_the_clicked_control() {
    let mut state = ready();
    state.open(1);
    state.scroll = 150.;
    state.pointer_focus(Target::App("netflix".into()));
    assert_eq!(state.scroll, 150.);
    state.focus(Target::Nav(1), &controls(&state));
    assert_eq!(state.scroll, 0.);
}

#[test]
fn gallery_right_crosses_rows_in_gallery_order() {
    let mut state = ready();
    state.open(2);
    state.section = 1;
    state.focus = Target::Background(2);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::Background(3));
}

#[test]
fn reordering_skips_hidden_and_unavailable_favorite_slots() {
    let mut state = ready();
    state.preferences.favorites = Some(vec![
        "netflix".into(),
        "missing".into(),
        "youtube.leanback.v4".into(),
        "amazon".into(),
    ]);
    state.preferences.hidden.push("youtube.leanback.v4".into());
    state.focus = Target::App("netflix".into());
    state.hold();
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(
        state.preferences.favorites.unwrap(),
        vec!["amazon", "missing", "youtube.leanback.v4", "netflix"]
    );
    assert_eq!(state.focus, Target::App("netflix".into()));
}

#[test]
fn focused_navigation_tabs_switch_pages_immediately() {
    let mut state = ready();
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.page, Page::Apps);
    assert_eq!(state.focus, Target::Nav(1));
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.page, Page::Settings);
    assert_eq!(state.focus, Target::Nav(2));
}

#[test]
fn greeting_subpage_opens_the_keyboard_and_back_restores_its_parent() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Widget(0);
    assert!(
        state
            .direction(Direction::Right, &controls(&state))
            .is_empty()
    );
    assert_eq!(state.subpage, Some(crate::state::Subpage::Greeting));
    assert_eq!(state.focus, Target::GreetingToggle);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::GreetingName);
    assert!(!state.text_input_active());
    assert!(state.activate().is_empty());
    assert!(state.text_input_active());
    state.text("Ana");
    assert_eq!(state.editing_text(), "Ana");
    assert!(state.preferences.greeting_name.is_empty());
    assert_eq!(state.finish_text_input(), vec![Effect::Save]);
    assert_eq!(state.focus, Target::GreetingName);
    assert!(state.finish_text_input().is_empty());
    assert_eq!(
        crate::i18n::Language::En.greeting(9, &state.preferences.greeting_name),
        "Good morning, Ana."
    );
    assert_eq!(state.subpage, Some(crate::state::Subpage::Greeting));
    assert!(state.back().is_empty());
    assert!(state.subpage.is_none());
    assert_eq!(state.focus, Target::Widget(0));
}

#[test]
fn greeting_name_replacement_commits_on_every_keyboard_completion_route() {
    for route in 0..3 {
        let mut state = ready();
        state.preferences.greeting_name = "Ana".into();
        state.open(2);
        state.focus = Target::Widget(0);
        state.activate();
        state.direction(Direction::Down, &controls(&state));
        state.activate();
        assert!(state.editing_text().is_empty());
        state.text("  BobX ");
        state.delete_surrounding_text(-3, 3);
        assert_eq!(state.editing_text(), "  Bo");
        state.text("b  ");
        assert_eq!(state.preferences.greeting_name, "Ana");
        let effects = match route {
            0 => {
                state.overlay = Some(Overlay::SystemKeyboard);
                state.finish_text_input()
            }
            1 => state.back(),
            _ => {
                state.focus = Target::Key(crate::state::KEYS.len() - 1);
                state.activate()
            }
        };
        assert_eq!(effects, vec![Effect::Save]);
        assert_eq!(state.preferences.greeting_name, "Bob");
        assert_eq!(state.focus, Target::GreetingName);
        assert!(state.finish_text_input().is_empty());
        state.activate();
        state.text("   ");
        assert!(state.back().is_empty());
        assert_eq!(state.preferences.greeting_name, "Bob");
    }
}

#[test]
fn remote_can_clear_a_saved_greeting_name_without_moving_the_controls() {
    let mut state = ready();
    state.preferences.greeting_name = "Ana".into();
    state.open(2);
    state.focus = Target::Widget(0);
    state.activate();
    let before: Vec<_> = controls(&state)
        .iter()
        .map(|c| (c.target.clone(), c.rect.y))
        .collect();
    state.direction(Direction::Down, &controls(&state));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::GreetingNameClear);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.greeting_name.is_empty());
    assert_eq!(state.focus, Target::GreetingName);
    let after: Vec<_> = controls(&state)
        .iter()
        .map(|c| (c.target.clone(), c.rect.y))
        .collect();
    assert_eq!(before, after);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Widget(0));
    assert!(state.subpage.is_none());
}

#[test]
fn app_name_settings_use_independent_defaults_including_with_legacy_preferences() {
    let old: Preferences = serde_json::from_value(json!({"show_names": true})).unwrap();
    assert!(!old.show_home_names);
    assert!(old.show_app_names);
    let mut state = ready();
    assert!(!state.preferences.show_home_names);
    assert!(state.preferences.show_app_names);
    state.open(2);
    state.focus = Target::Setting(0);
    assert!(
        state
            .direction(Direction::Right, &controls(&state))
            .is_empty()
    );
    assert_eq!(state.subpage, Some(crate::state::Subpage::AppNames));
    assert_eq!(state.focus, Target::AppNamesHome);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.show_home_names);
    assert!(state.preferences.show_app_names);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::AppNamesApps);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.show_home_names);
    assert!(!state.preferences.show_app_names);
    assert_eq!(state.subpage, Some(crate::state::Subpage::AppNames));
    let restored: Preferences =
        serde_json::from_value(serde_json::to_value(&state.preferences).unwrap()).unwrap();
    assert!(restored.show_home_names);
    assert!(!restored.show_app_names);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Setting(0));
    assert!(state.subpage.is_none());
}

#[test]
fn home_and_apps_render_only_their_own_caption_preference() {
    let mut state = ready();
    for (home, apps) in [(false, true), (true, false), (true, true), (false, false)] {
        state.preferences.show_home_names = home;
        state.preferences.show_app_names = apps;
        for (page, expected) in [(0, home), (1, apps)] {
            state.open(page);
            let scene = scene(&state);
            let caption = scene.draw.iter().any(
                |draw| matches!(draw, crate::renderer::Draw::Text(value, ..) if value == "Netflix"),
            );
            assert_eq!(caption, expected, "Home={home}, Apps={apps}, page={page}");
            let card = scene
                .controls
                .iter()
                .find(|c| c.target == Target::App("netflix".into()))
                .unwrap();
            assert_eq!(
                card.rect.h > 154.4,
                expected,
                "Hidden captions must not reserve space"
            );
        }
    }
}

#[test]
fn input_name_actions_only_highlight_the_focused_row_and_keep_the_saved_mode() {
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state
        .preferences
        .set_input_name("com.webos.app.hdmi1", "Console");
    state.open(2);
    state.focus = Target::Widget(3);
    state.activate();
    state.focus = Target::InputMode("com.webos.app.hdmi1".into());
    state.activate();
    for target in [Target::InputRename, Target::InputNameReset] {
        state.pointer_focus(target.clone());
        let scene = scene(&state);
        let highlighted: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| match draw {
                crate::renderer::Draw::RoundedGradient(rect, ..)
                    if rect.x == 640. && rect.w == 1120. =>
                {
                    Some(rect.y)
                }
                _ => None,
            })
            .collect();
        let focused = scene.controls.iter().find(|c| c.target == target).unwrap();
        assert_eq!(highlighted, vec![focused.rect.y]);
        assert!(scene.draw.iter().any(|draw| matches!(draw, crate::renderer::Draw::TintedImage(path, ..) if path.ends_with("check.lvec"))));
        assert_eq!(
            state
                .preferences
                .input_modes
                .get("com.webos.app.hdmi1")
                .copied()
                .unwrap_or_default(),
            InputMode::Auto
        );
        assert!(scene.draw.iter().any(
            |draw| matches!(draw, crate::renderer::Draw::Text(value, ..) if value == "Clear name")
        ));
    }
}

#[test]
fn clock_choices_only_highlight_focus_while_preserving_saved_checkmarks() {
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state.preferences.show_seconds = true;
    state.open(2);
    state.focus = Target::Setting(1);
    state.activate();
    state.activate();
    for index in 0..2 {
        state.pointer_focus(Target::Choice(index));
        let scene = scene(&state);
        let highlighted: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| match draw {
                crate::renderer::Draw::RoundedGradient(rect, ..)
                    if rect.x == 640. && rect.w == 1120. =>
                {
                    Some(rect.y)
                }
                _ => None,
            })
            .collect();
        let focused = scene
            .controls
            .iter()
            .find(|c| c.target == Target::Choice(index))
            .unwrap();
        assert_eq!(highlighted, vec![focused.rect.y]);
        let checks = scene.draw.iter().filter(|draw| matches!(draw, crate::renderer::Draw::TintedImage(path, ..) if path.ends_with("check.lvec"))).count();
        assert_eq!(checks, 1);
        assert!(!state.preferences.hour_24);
        assert!(state.preferences.show_seconds);
    }
}

#[test]
fn clock_format_subpage_saves_and_restores_its_parent_row() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Setting(1);
    assert!(
        state
            .direction(Direction::Right, &controls(&state))
            .is_empty()
    );
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert_eq!(state.focus, Target::ClockFormat);
    assert!(
        state
            .direction(Direction::Right, &controls(&state))
            .is_empty()
    );
    assert_eq!(state.subpage, Some(crate::state::Subpage::ClockFormat));
    assert_eq!(state.focus, Target::Choice(0));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.hour_24);
    assert!(!state.preferences.show_seconds);
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert_eq!(state.focus, Target::ClockFormat);
    state.direction(Direction::Left, &controls(&state));
    assert!(state.subpage.is_none());
    assert_eq!(state.focus, Target::Setting(1));
    state.activate();
    state.activate();
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert_eq!(state.focus, Target::ClockFormat);
    state.back();
    assert_eq!(state.focus, Target::Setting(1));
    assert!(state.subpage.is_none());
}

#[test]
fn time_format_selects_seconds_without_changing_the_hour_format() {
    let mut state = ready();
    state.preferences.hour_24 = true;
    state.open(2);
    state.focus = Target::Setting(1);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert!(!state.preferences.show_seconds);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::ClockTimeFormat);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.subpage, Some(crate::state::Subpage::TimeFormat));
    assert_eq!(state.focus, Target::Choice(0));
    assert!(
        !controls(&state)
            .iter()
            .any(|c| c.target == Target::Choice(2))
    );
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Choice(1));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.show_seconds);
    assert!(state.preferences.hour_24);
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert_eq!(state.focus, Target::ClockTimeFormat);
    state.activate();
    assert_eq!(state.focus, Target::Choice(1));
    state.back();
    assert_eq!(state.subpage, Some(crate::state::Subpage::Clock));
    assert_eq!(state.focus, Target::ClockTimeFormat);
    state.activate();
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(!state.preferences.show_seconds);
    assert!(state.preferences.hour_24);
    state.back();
    assert!(state.subpage.is_none());
    assert_eq!(state.focus, Target::Setting(1));
}

#[test]
fn system_version_is_inside_the_update_status_card_without_a_separate_row() {
    let mut state = ready();
    state.open(2);
    state.section = crate::state::SYSTEM_SECTION;
    state.panel_started = None;
    let system = scene(&state);
    assert!(system.draw.iter().any(|draw| matches!(
        draw,
        crate::renderer::Draw::Shape(rect, _, _, color)
            if rect.x == 640. && rect.y == 164.8 && rect.w == 1120. && rect.h == 220. && color[3] == 0.72
    )));
    assert!(system.draw.iter().any(|draw| matches!(
        draw,
        crate::renderer::Draw::Text(label, _, style, color, _)
            if label == "Current version: test" && style.size == 20.8 && style.weight == 450 && color[3] == 0.73
    )));
    assert_eq!(state.hit_test(&system.controls, 900., 200.), None);
    state.focus = Target::Sidebar(crate::state::SYSTEM_SECTION);
    state.direction(Direction::Right, &system.controls);
    assert_eq!(state.focus, Target::CheckUpdates);
}

#[test]
fn system_category_owns_refresh_and_quit_and_restores_sidebar_focus() {
    let mut state = ready();
    state.open(2);
    assert!(
        !controls(&state)
            .iter()
            .any(|c| matches!(c.target, Target::Setting(3 | 5)))
    );
    assert!(
        controls(&state)
            .iter()
            .any(|c| c.target == Target::Sidebar(crate::state::SYSTEM_SECTION))
    );
    state.focus = Target::Sidebar(crate::state::SYSTEM_SECTION - 1);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(crate::state::SYSTEM_SECTION));
    assert_eq!(state.section, crate::state::SYSTEM_SECTION);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::CheckUpdates);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Setting(3));
    assert_eq!(state.activate(), vec![Effect::Reload]);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Setting(7));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Setting(crate::state::TV_INPUT_ROW));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Setting(5));
    assert_eq!(state.activate(), vec![Effect::Quit]);
    state.back();
    assert_eq!(state.focus, Target::Sidebar(crate::state::SYSTEM_SECTION));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(0));
    assert_eq!(state.section, 0);
}

#[test]
fn text_rendering_preferences_default_to_msaa_and_recover_invalid_values() {
    use crate::text_rendering::TextRendering;
    for value in [
        json!({}),
        json!({"text_rendering": "unknown"}),
        json!({"text_rendering": null}),
        json!({"text_rendering": 4}),
    ] {
        let mut value = value;
        value["greeting_name"] = json!("Ana");
        let preferences: Preferences = serde_json::from_value(value).unwrap();
        assert_eq!(preferences.text_rendering, TextRendering::Msaa);
        assert_eq!(preferences.greeting_name, "Ana");
    }
}

#[test]
fn text_rendering_choices_persist_and_back_restores_the_system_row() {
    use crate::{state::Subpage, text_rendering::TextRendering};
    let mut state = ready();
    state.open(2);
    state.section = crate::state::SYSTEM_SECTION;
    state.focus = Target::Setting(7);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.subpage, Some(Subpage::TextRendering));
    assert_eq!(state.focus, Target::Choice(0));
    assert!(scene(&state).draw.iter().any(|draw| matches!(
        draw,
        crate::renderer::Draw::Text(label, ..) if label == "Restart Astro to apply changes."
    )));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Choice(1));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert_eq!(state.preferences.text_rendering, TextRendering::Analytic);
    assert_eq!(state.focus, Target::Setting(7));
    assert!(state.subpage.is_none());
    let saved = serde_json::to_value(&state.preferences).unwrap();
    assert_eq!(saved["text_rendering"], "analytic");
    let mut restored = State::new(serde_json::from_value(saved).unwrap());
    restored.open(2);
    restored.section = crate::state::SYSTEM_SECTION;
    restored.focus = Target::Setting(7);
    restored.activate();
    assert_eq!(restored.focus, Target::Choice(1));
    restored.direction(Direction::Down, &controls(&restored));
    assert_eq!(restored.focus, Target::Choice(0));
    restored.back();
    assert_eq!(restored.focus, Target::Setting(7));
    assert_eq!(restored.preferences.text_rendering, TextRendering::Analytic);
    restored.activate();
    restored.direction(Direction::Up, &controls(&restored));
    assert_eq!(restored.activate(), vec![Effect::Save]);
    assert_eq!(restored.preferences.text_rendering, TextRendering::Msaa);
}

#[test]
fn input_visibility_has_explicit_choices_and_returns_to_the_same_input() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Widget(3);
    state.activate();
    let id = "com.webos.app.hdmi1".to_string();
    state.focus = Target::InputMode(id.clone());
    state.activate();
    assert_eq!(state.focus, Target::Choice(2));
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert_eq!(state.preferences.input_modes[&id], InputMode::Off);
    assert_eq!(state.focus, Target::InputMode(id.clone()));
    assert!(!state.preferences.input_visible(&state.inventory.inputs[0]));
    state.activate();
    state.back();
    assert_eq!(state.focus, Target::InputMode(id));
    assert_eq!(state.subpage, Some(crate::state::Subpage::Inputs));
    state.back();
    assert_eq!(state.focus, Target::Widget(3));
}

#[test]
fn background_sidebar_right_focuses_upload_and_ok_restores_the_applied_background() {
    let mut state = ready();
    state.open(2);
    state.preferences.background = "forest".into();
    state.focus = Target::Sidebar(1);
    state.section = 1;
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::UploadWallpaper);
    assert_eq!(state.preferences.background, "forest");
    assert!(!state.preferences.wallpaper_cycle);
    assert!(state.overlay.is_none());
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::WallpaperGradient);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::WallpaperCycle);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::WallpaperGradient);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::UploadWallpaper);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(1));
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::UploadWallpaper);
    state.back();
    state.activate();
    assert_eq!(state.focus, Target::Background(6));
}

#[test]
fn categories_and_search_have_clear_actions_and_conditional_hidden_control() {
    let mut state = ready();
    state.open(1);
    assert!(
        !controls(&state)
            .iter()
            .any(|c| c.target == Target::Filter(5))
    );
    state.focus = Target::Search;
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Filter(4));
    state.preferences.hidden.push("netflix".into());
    state.focus = Target::Search;
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Filter(5));
    state.activate();
    assert_eq!(state.apps().len(), 1);
    state.focus = Target::ClearFilter;
    state.activate();
    assert_eq!(state.filter, 0);
    assert_eq!(state.focus, Target::Filter(1));
    state.query = "YouTube".into();
    state.focus = Target::ClearSearch;
    state.activate();
    assert!(state.query.is_empty());
    assert_eq!(state.focus, Target::Search);
}

#[test]
fn a_notice_does_not_escape_modal_focus_and_closing_restores_the_app() {
    let mut state = ready();
    state.open(1);
    state.notify("Saved");
    state.focus = Target::App("netflix".into());
    state.options();
    assert!(
        controls(&state)
            .iter()
            .all(|c| matches!(c.target, Target::OpenApp | Target::Option(_)))
    );
    state.focus = Target::Option(1);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Option(2));
    state.back();
    assert_eq!(state.focus, Target::App("netflix".into()));
}

#[test]
fn delete_confirmation_defaults_to_cancel_and_back_restores_side_popup() {
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state.open(1);
    state.focus = Target::App("netflix".into());
    let card = controls(&state)
        .into_iter()
        .find(|c| c.target == state.focus)
        .unwrap()
        .rect;
    let parent_draw_count = scene(&state).draw.len();
    state.options();
    let popup_scene = scene(&state);
    let popup = popup_scene.modal.unwrap();
    assert!(
        popup_scene
            .controls
            .iter()
            .all(|control| matches!(control.target, Target::OpenApp | Target::Option(_)))
    );
    assert!(!popup_scene.draw[parent_draw_count..].iter().any(|draw| matches!(draw,
        crate::renderer::Draw::Text(value, rect, ..) if popup.contains(rect.x, rect.y)
            && ["Netflix", "Streaming", "Select", "Close", "OK", "Back"].contains(&value.as_str()))));
    let action_icons: Vec<_> = popup_scene.draw[parent_draw_count..]
        .iter()
        .filter_map(|draw| match draw {
            crate::renderer::Draw::Image(_, rect, ..)
            | crate::renderer::Draw::TintedImage(_, rect, ..)
                if popup.contains(rect.x, rect.y) =>
            {
                Some(rect)
            }
            _ => None,
        })
        .collect();
    assert_eq!(action_icons.len(), 4);
    assert!(
        action_icons
            .iter()
            .all(|rect| rect.x > popup.x + popup.w - 80.)
    );
    assert!(popup.x >= card.x + card.w || popup.x + popup.w <= card.x);
    state.focus = Target::Option(2);
    let focused_scene = scene(&state);
    let delete_rect = focused_scene
        .controls
        .iter()
        .find(|c| c.target == Target::Option(2))
        .unwrap()
        .rect;
    assert!(focused_scene.draw.iter().any(|draw| matches!(draw,
        crate::renderer::Draw::Shape(rect, _, _, color) if rect.x == delete_rect.x && rect.y == delete_rect.y
            && *color == [214. / 255., 59. / 255., 53. / 255., 1.])));
    assert!(focused_scene.draw.iter().any(|draw| matches!(draw,
        crate::renderer::Draw::Text(label, rect, _, color, _) if label == "Delete app" && delete_rect.contains(rect.x, rect.y)
            && *color == crate::renderer::WHITE)));
    assert!(state.activate().is_empty());
    assert!(matches!(state.overlay, Some(Overlay::Delete { .. })));
    assert_eq!(state.focus, Target::CancelDelete);
    let confirmation = scene(&state).modal.unwrap();
    assert_eq!(confirmation.w, 640.);
    assert!(confirmation.h < confirmation.w);
    assert!(
        controls(&state)
            .iter()
            .all(|c| matches!(c.target, Target::ConfirmDelete | Target::CancelDelete))
    );
    assert!(state.activate().is_empty());
    assert!(matches!(state.overlay, Some(Overlay::Options { .. })));
    assert_eq!(state.focus, Target::Option(2));
    state.activate();
    state.back();
    assert_eq!(state.focus, Target::Option(2));
    state.back();
    assert_eq!(state.focus, Target::App("netflix".into()));
}

#[test]
fn opening_side_popup_preserves_card_artwork_geometry_with_or_without_captions() {
    for page in [0, 1] {
        for names in [false, true] {
            let mut state = ready();
            state.preferences.show_app_names = names;
            state.preferences.show_home_names = names;
            state.open(page);
            state.focus = Target::App("netflix".into());
            let before = scene(&state);
            let logo_rect = |draws: &[crate::renderer::Draw]| {
                draws
                    .iter()
                    .filter_map(|draw| match draw {
                        crate::renderer::Draw::Image(path, rect, ..)
                        | crate::renderer::Draw::TintedImage(path, rect, ..)
                            if path.contains("netflix") =>
                        {
                            Some([rect.x, rect.y, rect.w, rect.h])
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let original = logo_rect(&before.draw);
            assert!(!original.is_empty());
            state.options();
            let lifted = logo_rect(&scene(&state).draw);
            assert_eq!(lifted.len(), original.len() * 2);
            for (actual, expected) in lifted.iter().zip(original.iter().cycle()) {
                for (a, b) in actual.iter().zip(expected) {
                    assert!(
                        (a - b).abs() < 0.001,
                        "Artwork bounds changed: {actual:?} vs {expected:?}"
                    );
                }
            }
            let hearts = |draws: &[crate::renderer::Draw]| {
                draws
                    .iter()
                    .filter_map(|draw| match draw {
                        crate::renderer::Draw::Image(path, rect, ..)
                        | crate::renderer::Draw::TintedImage(path, rect, ..)
                            if path.contains("heart-filled") && rect.x < 350. =>
                        {
                            Some([rect.x, rect.y, rect.w, rect.h])
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let original_hearts = hearts(&before.draw);
            assert_eq!(original_hearts.len(), usize::from(page == 1));
            let popup_hearts = hearts(&scene(&state).draw);
            assert_eq!(popup_hearts.len(), original_hearts.len() + 1);
            if page == 1 {
                assert_eq!(popup_hearts, vec![original_hearts[0], original_hearts[0]]);
            }
        }
    }
}

#[test]
fn protected_apps_cannot_be_deleted_through_ui_or_worker() {
    for id in [
        crate::platform::APP_ID,
        crate::platform::HOME_ID,
        "com.webos.app.livetv",
        "com.webos.app.browser",
        "com.webos.app.settings",
        "com.webos.service.applicationmanager",
        "com.palm.app.settings",
        "com.palm.app.input",
        "com.lge.app.store",
        "com.lge.service.example",
        "com.ecklf.luma",
        "com.luma.launcher",
        "com.luma.native",
        "com.luma.launcher.native",
        "org.webosbrew.hbchannel",
        "org.webosbrew.hbchannel.service",
        "invalid';command",
    ] {
        assert!(crate::platform::uninstall_request(id).is_err());
        let mut state = ready();
        state.inventory.apps.push(App::new(id, "Renamed app"));
        state.open(1);
        state.focus = Target::App(id.into());
        state.options();
        assert!(
            !controls(&state)
                .iter()
                .any(|c| c.target == Target::Option(2))
        );
        state.focus = Target::Option(2);
        assert!(state.activate().is_empty());
        assert!(matches!(state.overlay, Some(Overlay::Options { .. })));
        state.overlay = Some(Overlay::Delete {
            id: id.into(),
            restore: Target::App(id.into()),
        });
        state.focus = Target::ConfirmDelete;
        assert!(state.activate().is_empty());
    }
    assert_eq!(
        crate::platform::uninstall_request("netflix").unwrap(),
        (
            "luna://com.webos.appInstallService/remove",
            json!({"id": "netflix", "subscribe": true})
        )
    );
    for id in [
        "com.example.app",
        "com.webosbrew.app",
        "com.palmtree.app",
        "com.lgeneric.app",
    ] {
        assert!(
            crate::platform::can_uninstall(id),
            "Unexpectedly protected: {id}"
        );
    }
}

#[test]
fn deletion_is_single_flight_and_only_success_cleans_up_preferences() {
    let mut state = ready();
    state.open(1);
    state.preferences.favorites = Some(vec!["netflix".into(), "youtube.leanback.v4".into()]);
    state.preferences.hidden.push("netflix".into());
    state.focus = Target::App("netflix".into());
    state.options();
    state.focus = Target::Option(2);
    state.activate();
    state.focus = Target::ConfirmDelete;
    let cancel_before = controls(&state)
        .into_iter()
        .find(|c| c.target == Target::CancelDelete)
        .unwrap()
        .rect;
    assert_eq!(state.activate(), vec![Effect::Uninstall("netflix".into())]);
    assert_eq!(state.focus, Target::CancelDelete);
    let cancel_after = controls(&state)
        .into_iter()
        .find(|c| c.target == Target::CancelDelete)
        .unwrap()
        .rect;
    assert_eq!(
        [
            cancel_before.x,
            cancel_before.y,
            cancel_before.w,
            cancel_before.h
        ],
        [
            cancel_after.x,
            cancel_after.y,
            cancel_after.w,
            cancel_after.h
        ]
    );
    assert!(
        !controls(&state)
            .iter()
            .any(|c| c.target == Target::ConfirmDelete)
    );
    state.focus = Target::ConfirmDelete;
    assert!(state.activate().is_empty());
    assert!(state.uninstall_response("other".into(), Ok(())).is_empty());
    assert_eq!(state.uninstalling.as_deref(), Some("netflix"));
    assert_eq!(
        state.uninstall_response("netflix".into(), Err("denied".into())),
        vec![Effect::Refresh]
    );
    assert!(state.inventory.apps.iter().any(|a| a.id == "netflix"));
    assert!(state.preferences.hidden.contains(&"netflix".into()));
    assert_eq!(state.preferences.favorites.as_ref().unwrap().len(), 2);
    assert_eq!(state.focus, state.first_content());
    assert!(state.overlay.is_none());
    state.focus = Target::App("netflix".into());
    state.options();
    state.focus = Target::Option(2);
    state.activate();
    state.focus = Target::ConfirmDelete;
    state.activate();
    assert_eq!(
        state.uninstall_response("netflix".into(), Ok(())),
        vec![Effect::Save, Effect::Refresh]
    );
    assert!(!state.inventory.apps.iter().any(|a| a.id == "netflix"));
    assert!(state.preferences.hidden.is_empty());
    assert_eq!(
        state.preferences.favorites,
        Some(vec!["youtube.leanback.v4".into()])
    );
    assert!(state.overlay.is_none());
    assert_eq!(state.focus, Target::App("youtube.leanback.v4".into()));
}

#[test]
fn deletion_completion_does_not_steal_focus_after_dismissal() {
    let mut state = ready();
    state.uninstalling = Some("netflix".into());
    state.open(2);
    state.uninstall_response("netflix".into(), Ok(()));
    assert_eq!(state.page, Page::Settings);
    assert_eq!(state.focus, Target::Nav(2));
    assert!(state.overlay.is_none());
}

#[test]
fn app_activation_opens_options_with_open_focused_and_next_ok_launches() {
    for page in [0, 1] {
        let mut state = ready();
        state.open(page);
        state.focus = Target::App("netflix".into());
        if page == 0 {
            assert_eq!(state.activate(), vec![Effect::Launch("netflix".into())]);
            assert!(state.overlay.is_none());
            continue;
        }
        assert!(state.activate().is_empty());
        assert!(matches!(state.overlay, Some(Overlay::Options { .. })));
        assert_eq!(state.focus, Target::OpenApp);
        assert_eq!(state.activate(), vec![Effect::Launch("netflix".into())]);
        assert!(state.overlay.is_none());
        assert_eq!(state.focus, Target::App("netflix".into()));
    }
}

#[test]
fn uncertain_deletion_uses_complete_inventory_instead_of_visible_app_filtering() {
    use crate::platform::inventory_confirms_removal;
    assert_eq!(inventory_confirms_removal(&json!([]), "netflix"), Ok(true));
    assert_eq!(
        inventory_confirms_removal(&json!([{"id": "youtube.leanback.v4"}]), "netflix"),
        Ok(true)
    );
    assert_eq!(
        inventory_confirms_removal(&json!([{"id": "netflix", "visible": false}]), "netflix"),
        Ok(false)
    );
    assert!(inventory_confirms_removal(&json!({}), "netflix").is_err());
    assert!(inventory_confirms_removal(&json!([{}]), "netflix").is_err());
}

#[test]
fn failed_deletion_refreshes_inventory_and_closes_stale_confirmation() {
    let mut state = ready();
    state.open(1);
    state.focus = Target::App("netflix".into());
    state.options();
    state.focus = Target::Option(2);
    state.activate();
    state.focus = Target::ConfirmDelete;
    state.activate();
    assert_eq!(
        state.uninstall_response("netflix".into(), Err("timeout".into())),
        vec![Effect::Refresh]
    );
    assert!(state.overlay.is_none());
    assert_eq!(state.focus, Target::App("netflix".into()));
    let mut inventory = demo_inventory();
    inventory.apps.retain(|app| app.id != "netflix");
    state.set_inventory(inventory, None);
    assert!(!state.apps().iter().any(|app| app.id == "netflix"));
    assert_ne!(state.focus, Target::App("netflix".into()));
}

#[test]
fn remote_toolbar_focus_normalizes_scroll_but_hover_does_not() {
    let mut state = ready();
    state.open(1);
    state.scroll = 100.;
    state.pointer_focus(Target::Filter(1));
    assert_eq!(state.scroll, 100.);
    state.focus(Target::Filter(2), &controls(&state));
    assert_eq!(state.scroll, 0.);
}

#[test]
fn long_captions_expand_rows_without_overlapping_the_next_row() {
    let mut state = ready();
    state.open(1);
    state.inventory.apps.iter_mut().find(|app| app.id == "netflix").unwrap().title = "A very long installed application title that needs more than two lines to remain fully readable".into();
    let targets = controls(&state);
    let title = targets
        .iter()
        .find(|c| c.target == Target::App("netflix".into()))
        .unwrap();
    let next_id = state.apps()[6].id.clone();
    let next = targets
        .iter()
        .find(|c| c.target == Target::App(next_id.clone()))
        .unwrap();
    assert!(title.rect.h > 240.);
    assert!(next.rect.y > title.rect.y + title.rect.h);
}

#[test]
fn reduced_motion_disables_panel_wallpaper_and_notification_animation() {
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state.open(2);
    state.focus = Target::Setting(1);
    state.activate();
    assert!(!state.animating());
    state.back();
    state.focus = Target::Background(6);
    state.activate();
    state.notify("Saved");
    assert!(!state.animating());
}

#[test]
fn pinning_a_hidden_app_restores_it_and_leaves_hidden_when_empty() {
    let mut state = ready();
    state.open(1);
    state.filter = 5;
    state.preferences.hidden.push("org.jellyfin.webos".into());
    state.focus = Target::App("org.jellyfin.webos".into());
    state.options();
    assert_eq!(state.focus, Target::OpenApp);
    state.focus = Target::Option(0);
    state.activate();
    assert!(state.preferences.hidden.is_empty());
    assert_eq!(state.filter, 0);
    assert!(
        state
            .preferences
            .favorites(&state.inventory.apps)
            .contains(&"org.jellyfin.webos".into())
    );
    assert_eq!(state.focus, Target::App("org.jellyfin.webos".into()));
}

#[test]
fn language_selection_restores_its_parent_and_is_saved_with_preferences() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Setting(4);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.subpage, Some(crate::state::Subpage::Language));
    assert_eq!(
        state.focus,
        Target::Choice(crate::i18n::Language::En.index())
    );
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert_eq!(state.preferences.language, crate::i18n::Language::Es);
    assert_eq!(state.focus, Target::Setting(4));
    state.activate();
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Setting(4));
}

#[test]
fn remote_can_select_and_restore_every_language() {
    for (index, language) in crate::i18n::Language::ALL.into_iter().enumerate() {
        let mut state = ready();
        state.open(2);
        state.focus = Target::Setting(4);
        state.activate();
        for _ in 0..crate::i18n::Language::En.index() {
            state.direction(Direction::Up, &controls(&state));
        }
        for _ in 0..index {
            state.direction(Direction::Down, &controls(&state));
        }
        assert_eq!(state.focus, Target::Choice(index));
        let focused = controls(&state)
            .into_iter()
            .find(|control| control.target == state.focus)
            .unwrap();
        let viewport = state.content_viewport();
        assert!(focused.rect.y >= viewport.y);
        assert!(focused.rect.y + focused.rect.h <= viewport.y + viewport.h);
        assert_eq!(state.activate(), vec![Effect::Save]);
        assert_eq!(state.preferences.language, language);
        assert_eq!(state.focus, Target::Setting(4));

        let saved = serde_json::to_value(&state.preferences).unwrap();
        let mut restored = State::new(serde_json::from_value(saved).unwrap());
        restored.open(2);
        restored.focus = Target::Setting(4);
        restored.activate();
        assert_eq!(restored.preferences.language, language);
        assert_eq!(restored.focus, Target::Choice(index));
        restored.direction(Direction::Down, &controls(&restored));
        assert_eq!(
            restored.focus,
            Target::Choice((index + 1) % crate::i18n::Language::ALL.len())
        );
    }
}

#[test]
fn settings_order_and_language_sort() {
    use crate::i18n::Language;
    assert!(
        Language::ALL
            .windows(2)
            .all(|pair| pair[0].name().to_lowercase() < pair[1].name().to_lowercase())
    );
    for (index, language) in Language::ALL.into_iter().enumerate() {
        assert_eq!(language.index(), index);
    }
    let mut state = ready();
    state.open(2);
    let general = controls(&state);
    let language = general
        .iter()
        .find(|control| control.target == Target::Setting(4))
        .unwrap();
    let motion = general
        .iter()
        .find(|control| control.target == Target::Setting(2))
        .unwrap();
    assert!(language.rect.y < motion.rect.y);
}

#[test]
fn unsupported_language_preferences_recover_without_losing_other_choices() {
    // Includes codes of languages that earlier versions offered.
    for language in ["unknown", "de", "zh-tw", "cs"] {
        let preferences: Preferences = serde_json::from_value(
            json!({"language":language,"favorites":["netflix"],"show_home_names":true}),
        )
        .unwrap();
        assert_eq!(preferences.language, crate::i18n::Language::En);
        assert!(preferences.show_home_names);
        assert_eq!(preferences.favorites, Some(vec!["netflix".into()]));
    }
    let preferences: Preferences = serde_json::from_value(json!({"language":"es"})).unwrap();
    assert_eq!(preferences.language, crate::i18n::Language::Es);
}

#[test]
fn unrelated_app_ids_and_titles_keep_their_installed_artwork() {
    for (id, title) in [
        ("com.tvweb.dashboard", "Glasshouse"),
        ("com.example.ardent", "Notes"),
        ("com.example.3ard", "Notes"),
        ("com.example.ard3", "Notes"),
        ("com.example.plexus", "Notes"),
        ("com.example.media", "Standard dashboard"),
        ("com.example.media", "netflixer"),
        ("com.example.media", "preNetflix"),
        ("amazon.alexa.view", "Amazon Alexa"),
        ("com.example.spotify", "Spotify tools"),
        ("com.example.media", "Plex"),
        ("com.example.browser", "Web Browser"),
        ("com.example.livetv", "Live TV"),
    ] {
        let app = App::new(id, title);
        assert_eq!(
            (app.brand.as_str(), app.category.as_str()),
            ("generic", "Utilities"),
            "An unrelated application must not be rebranded: {id} / {title}"
        );
    }
}

#[test]
fn curated_branding_does_not_use_title_words_or_partial_id_matches() {
    for (id, title) in [
        ("com.example.ard_more", "Notes"),
        ("com.example.more-ard", "Notes"),
        ("com.example.ARD", "Notes"),
        ("com.example.media", "ARD"),
        ("com.example.media", "(ARD)"),
        ("com.example.media", "ARD미디어테크"),
        ("com.example.media", "아에르데ARD"),
        ("com.example.media", "아에르데ARD미디어테크"),
        ("com.example.dashboard", "ARD"),
        ("com.example.media", "standard ardent ARD"),
    ] {
        let app = App::new(id, title);
        assert_eq!(
            (app.brand.as_str(), app.category.as_str()),
            ("generic", "Utilities"),
            "Only an exact app ID may select curated artwork: {id} / {title}"
        );
    }
}

#[test]
fn known_app_ids_preserve_the_curated_brands_and_categories() {
    let apps = demo_inventory().apps;
    let expected = [
        ("com.webos.app.livetv", "live-tv", "Streaming"),
        ("netflix", "netflix", "Streaming"),
        ("youtube.leanback.v4", "youtube", "Streaming"),
        ("com.apple.appletv", "apple-tv", "Streaming"),
        ("com.disney.disneyplus-prod", "disney-plus", "Streaming"),
        ("amazon", "prime-video", "Streaming"),
        ("plex", "plex", "Streaming"),
        ("spotify-beehive", "spotify", "Music"),
        ("org.jellyfin.webos", "jellyfin", "Streaming"),
        ("org.xbmc.kodi", "kodi", "Streaming"),
        ("com.limelight.webos", "moonlight", "Gaming"),
        ("org.webosbrew.hbchannel", "homebrew", "Utilities"),
        ("com.webos.app.browser", "browser", "Utilities"),
        ("com.arte.tv", "arte", "Streaming"),
        ("de.zdf.app.zdfm3", "zdf", "Streaming"),
        ("ard.mediathek", "das-erste", "Streaming"),
    ];
    for (id, brand, category) in expected {
        let app = apps.iter().find(|app| app.id == id).unwrap();
        assert_eq!(
            (app.brand.as_str(), app.category.as_str()),
            (brand, category),
            "Known app {id} must retain its curated brand and category"
        );
    }
}

#[test]
fn hdmi_plug_in_reports_the_cable_state_when_connected_only_means_present() {
    let inputs = normalize_inputs(&json!([
        {"id":"HDMI_1","label":"HDMI 1","port":1,"connected":true,"appId":"com.webos.app.hdmi1","hdmiPlugIn":false},
        {"id":"HDMI_2","label":"AVR-X1800H","port":2,"connected":true,"appId":"com.webos.app.hdmi2","hdmiPlugIn":true},
        {"id":"HDMI_3","label":"HDMI 3","port":3,"connected":true,"appId":"com.webos.app.hdmi3","hdmiPlugIn":false},
        {"id":"HDMI_4","label":"HDMI 4","port":4,"connected":true,"appId":"com.webos.app.hdmi4","hdmiPlugIn":false}
    ]))
    .unwrap();
    let connected: Vec<_> = inputs
        .iter()
        .map(|input| (input.id.as_str(), input.connected))
        .collect();
    assert_eq!(
        connected,
        [
            ("com.webos.app.hdmi1", Some(false)),
            ("com.webos.app.hdmi2", Some(true)),
            ("com.webos.app.hdmi3", Some(false)),
            ("com.webos.app.hdmi4", Some(false)),
        ]
    );
    assert_eq!(inputs[1].title, "AVR-X1800H");
    let preferences = Preferences::default();
    let visible: Vec<_> = inputs
        .iter()
        .filter(|input| preferences.input_visible(input))
        .map(|input| input.id.as_str())
        .collect();
    assert_eq!(visible, ["com.webos.app.hdmi2"]);
}

#[test]
fn inputs_without_hdmi_plug_in_keep_the_connected_report() {
    let inputs = normalize_inputs(&json!([
        {"appId":"com.webos.app.hdmi1","id":"HDMI_1","port":1,"connected":true},
        {"appId":"com.webos.app.hdmi2","id":"HDMI_2","port":2,"connected":false},
        {"appId":"com.webos.app.hdmi3","connected":true,"hdmiPlugIn":"yes"},
        {"appId":"com.webos.app.hdmi4"}
    ]))
    .unwrap();
    let connected: Vec<_> = inputs.iter().map(|input| input.connected).collect();
    assert_eq!(connected, [Some(true), Some(false), Some(true), None]);
}

#[test]
fn input_names_are_sanitized_and_old_preferences_load_without_them() {
    let old: Preferences = serde_json::from_value(json!({"background": "forest"})).unwrap();
    assert!(old.input_names.is_empty());
    let mut preferences: Preferences = serde_json::from_value(json!({
        "input_names": {
            "com.webos.app.hdmi1": "  Play\nStation\t5  ",
            "com.webos.app.hdmi2": "   ",
            "bad;id": "Ignored",
            "com.webos.app.hdmi3": "A".repeat(60),
        }
    }))
    .unwrap();
    preferences.sanitize();
    assert_eq!(
        preferences.input_names,
        BTreeMap::from([
            (
                "com.webos.app.hdmi1".to_string(),
                "PlayStation5".to_string()
            ),
            (
                "com.webos.app.hdmi3".to_string(),
                "A".repeat(crate::model::INPUT_NAME_LIMIT)
            ),
        ])
    );
    assert!(preferences.set_input_name("com.webos.app.hdmi2", " DENON "));
    assert!(!preferences.set_input_name("com.webos.app.hdmi2", "DENON"));
    assert_eq!(preferences.input_names["com.webos.app.hdmi2"], "DENON");
    assert!(preferences.set_input_name("com.webos.app.hdmi2", ""));
    assert!(!preferences.input_names.contains_key("com.webos.app.hdmi2"));
    assert!(!preferences.set_input_name("bad;id", "Name"));
}

#[test]
fn custom_input_names_keep_the_label_port_and_status_as_detail() {
    let mut preferences = Preferences::default();
    let input = Input {
        id: "com.webos.app.hdmi2".into(),
        title: "AVR-X1800H".into(),
        port: "HDMI 2".into(),
        connected: Some(true),
        signal: None,
    };
    let language = preferences.language;
    let status = |preferences: &Preferences| {
        view::input_status(&input, preferences.input_name(&input).is_some(), language)
    };
    assert_eq!(preferences.input_title(&input), "HDMI 2");
    assert_eq!(status(&preferences), "AVR-X1800H · Connected");
    preferences.set_input_name(&input.id, "DENON");
    assert_eq!(preferences.input_title(&input), "DENON");
    assert_eq!(status(&preferences), "AVR-X1800H · HDMI 2 · Connected");
    let plain = Input {
        title: "HDMI 2".into(),
        ..input.clone()
    };
    assert_eq!(
        view::input_status(&plain, true, language),
        "HDMI 2 · Connected"
    );
    assert_eq!(view::input_status(&plain, false, language), "Connected");
}

#[test]
fn remote_renames_and_resets_an_input_from_its_settings_page() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Widget(3);
    state.activate();
    let id = "com.webos.app.hdmi2".to_string();
    state.focus = Target::InputMode(id.clone());
    state.activate();
    assert_eq!(state.focus, Target::Choice(2));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::InputRename);
    assert!(
        !controls(&state)
            .iter()
            .any(|c| c.target == Target::InputNameReset)
    );
    state.activate();
    assert!(matches!(state.overlay, Some(Overlay::Keyboard)));
    state.text("DENONX");
    state.delete();
    assert_eq!(state.back(), vec![Effect::Save]);
    assert_eq!(state.focus, Target::InputRename);
    assert_eq!(state.preferences.input_names[&id], "DENON");
    assert_eq!(
        state.preferences.input_title(&state.inventory.inputs[1]),
        "DENON"
    );
    // Dismissing an empty keyboard keeps the saved name.
    state.activate();
    assert!(state.finish_text_input().is_empty());
    assert_eq!(state.preferences.input_names[&id], "DENON");
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::InputNameReset);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(state.preferences.input_names.is_empty());
    assert_eq!(state.focus, Target::InputRename);
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.focus, Target::Choice(2));
    state.focus = Target::InputRename;
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::InputMode(id));
    assert_eq!(state.subpage, Some(crate::state::Subpage::Inputs));
}

#[test]
fn keyboard_done_saves_a_typed_input_name() {
    let mut state = ready();
    state.open(2);
    state.focus = Target::Widget(3);
    state.activate();
    state.focus = Target::InputMode("com.webos.app.hdmi1".into());
    state.activate();
    state.focus = Target::InputRename;
    state.activate();
    state.text("PS5");
    state.focus = Target::Key(
        crate::state::KEYS
            .iter()
            .position(|k| *k == "Done")
            .unwrap(),
    );
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert_eq!(state.preferences.input_names["com.webos.app.hdmi1"], "PS5");
    assert!(state.overlay.is_none());
}

#[test]
fn home_lifecycle_replies_map_for_both_registration_versions() {
    use crate::platform::Lifecycle;
    // Lifecycle v2 (registerApp, in place of LG's Home executable) names it in "event".
    assert_eq!(
        Lifecycle::from_reply(&json!({"event": "relaunch", "reason": "hotKey"})),
        Some(Lifecycle::Relaunch)
    );
    assert_eq!(
        Lifecycle::from_reply(&json!({"event": "close"})),
        Some(Lifecycle::Close)
    );
    // Lifecycle v1 (registerNativeApp for a staged Home manifest on webOS 10/11) in "message".
    assert_eq!(
        Lifecycle::from_reply(&json!({
            "parameters": {"launchType": "homeKey", "activateType": "home"},
            "returnValue": true,
            "message": "relaunch"
        })),
        Some(Lifecycle::Relaunch)
    );
    assert_eq!(
        Lifecycle::from_reply(&json!({"returnValue": true, "message": "close"})),
        Some(Lifecycle::Close)
    );
    // Acknowledgements and keep-alive need no action.
    for reply in [
        json!({"returnValue": true, "message": "registered"}),
        json!({"event": "registered"}),
        json!({"event": "pause", "reason": "keepAlive"}),
        json!({"returnValue": false, "errorText": "refused"}),
    ] {
        assert_eq!(Lifecycle::from_reply(&reply), None, "{reply}");
    }
}

#[test]
fn foreground_check_needs_focus_after_a_relaunch_from_the_background() {
    use crate::platform::ForegroundCheck;
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let late = start + ForegroundCheck::TIMEOUT;

    // Relaunch while hidden, then only Shown (SDL emits it for SDL_ShowWindow itself): fails.
    let mut check = ForegroundCheck::default();
    check.relaunched(start, false);
    assert!(!check.failed(start + Duration::from_secs(1)));
    assert!(check.failed(late));
    // Reported once.
    assert!(!check.failed(late + Duration::from_secs(1)));

    // FocusGained or a remote key in time: no failure.
    let mut check = ForegroundCheck::default();
    check.relaunched(start, false);
    check.confirmed();
    assert!(!check.failed(late));

    // Already focused and visible (Home pressed on Home): nothing to check.
    let mut check = ForegroundCheck::default();
    check.relaunched(start, true);
    assert!(!check.failed(late));

    // A later relaunch from the background is checked again.
    check.relaunched(late, false);
    assert!(check.failed(late + ForegroundCheck::TIMEOUT));
}

#[test]
fn prewarm_waits_while_recents_replaces_the_page() {
    let mut state = ready();
    let mut jobs = vec![Page::Settings, Page::Apps];
    state.open_recents();
    state.recents.visibility = 1.;
    // As after the frame that showed it.
    state.dirty = false;
    // The Apps scene would be Recents only, so preparing it now would be wasted.
    let apps_controls = |state: &mut State| {
        let current = std::mem::replace(&mut state.page, Page::Apps);
        let found = controls(state)
            .iter()
            .any(|control| matches!(control.target, Target::App(_)));
        state.page = current;
        found
    };
    assert!(!apps_controls(&mut state));
    assert_eq!(crate::next_prewarm(&mut jobs, &state, true), None);
    assert_eq!(jobs, [Page::Settings, Page::Apps]);
    // Once Recents is gone the retained jobs run, in order.
    state.overlay = None;
    state.recents.visibility = 0.;
    state.dirty = false;
    assert!(apps_controls(&mut state));
    assert_eq!(crate::next_prewarm(&mut jobs, &state, false), None);
    assert_eq!(
        crate::next_prewarm(&mut jobs, &state, true),
        Some(Page::Apps)
    );
    assert_eq!(
        crate::next_prewarm(&mut jobs, &state, true),
        Some(Page::Settings)
    );
    assert_eq!(crate::next_prewarm(&mut jobs, &state, true), None);
}

#[test]
fn failed_first_inventory_keeps_loading_until_retries_are_used_up() {
    let mut state = State::new(Preferences::default());
    state.loading = true;
    for attempt in 1..=crate::state::INVENTORY_RETRIES {
        assert_eq!(
            state.inventory_failed("The TV service timed out".into()),
            Some(crate::state::INVENTORY_RETRY_DELAY)
        );
        assert_eq!(state.inventory_failures, attempt);
        assert!(state.loading && state.error.is_none());
    }
    assert_eq!(
        state.inventory_failed("The TV service timed out".into()),
        None
    );
    assert!(!state.loading);
    assert_eq!(state.error.as_deref(), Some("The TV service timed out"));
    assert_eq!(state.first_content(), Target::Retry);
    // Retry by hand starts a fresh round of automatic retries.
    assert_eq!(state.inventory_failures, 0);
}

#[test]
fn inventory_success_resets_failures_and_later_failures_show_at_once() {
    let mut state = State::new(Preferences::default());
    state.loading = true;
    assert!(state.inventory_failed("timeout".into()).is_some());
    state.set_inventory(demo_inventory(), None);
    assert_eq!(state.inventory_failures, 0);
    state.loading = true;
    // Apps are on screen already: a failed refresh is reported, not retried behind them.
    assert_eq!(state.inventory_failed("timeout".into()), None);
    assert!(!state.loading && state.error.is_some());
    assert!(!state.inventory.apps.is_empty());
}

#[test]
fn widgets_group_existing_settings_and_restore_each_parent_without_changing_preferences() {
    use crate::state::{SECTION_COUNT, Subpage, WIDGETS_SECTION};
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state.preferences.greeting_name = "Ana".into();
    let saved = serde_json::to_value(&state.preferences).unwrap();
    state.open(2);
    assert_eq!(SECTION_COUNT, 4);
    assert!(
        !controls(&state)
            .iter()
            .any(|control| control.target == Target::Setting(6))
    );
    state.direction(Direction::Down, &controls(&state));
    state.direction(Direction::Down, &controls(&state));
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(WIDGETS_SECTION));
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.focus, Target::Widget(4));
    let widget_order: Vec<_> = controls(&state)
        .into_iter()
        .filter_map(|control| match control.target {
            Target::Widget(index) => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(widget_order, [4, 0, 3]);
    for (index, subpage) in [
        (4, Subpage::Date),
        (0, Subpage::Greeting),
        (3, Subpage::Inputs),
    ] {
        state.focus = Target::Widget(index);
        state.direction(Direction::Right, &controls(&state));
        assert_eq!(state.subpage, Some(subpage));
        state.back();
        assert!(state.subpage.is_none());
        assert_eq!(state.focus, Target::Widget(index));
    }
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Sidebar(WIDGETS_SECTION));
    assert_eq!(serde_json::to_value(&state.preferences).unwrap(), saved);
    state.inventory.inputs.clear();
    state.focus = Target::Widget(3);
    state.activate();
    assert_eq!(state.subpage, Some(Subpage::Inputs));
    assert_eq!(state.focus, Target::Setting(3));
    assert_eq!(state.activate(), vec![Effect::Reload]);
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Widget(3));
}

#[test]
fn widgets_are_localized_without_legacy_sidebar_entries() {
    use crate::state::WIDGETS_SECTION;
    for language in crate::i18n::Language::ALL {
        let mut state = ready();
        state.preferences.language = language;
        state.open(2);
        state.pointer_focus(Target::Sidebar(WIDGETS_SECTION));
        let page = scene(&state);
        assert_eq!(
            page.controls
                .iter()
                .filter(|c| matches!(c.target, Target::Sidebar(_)))
                .count(),
            4
        );
        assert_eq!(
            page.controls
                .iter()
                .filter(|c| matches!(c.target, Target::Widget(_)))
                .count(),
            3
        );
        for key in ["widgets", "greeting", "inputs", "date"] {
            assert!(page.draw.iter().any(|draw| matches!(draw, crate::renderer::Draw::Text(label, ..) if label == language.t(key))), "{key}/{language:?}");
        }
        state.pointer_focus(Target::Sidebar(0));
        assert!(!scene(&state).draw.iter().any(|draw| matches!(draw, crate::renderer::Draw::Text(label, ..) if label == language.t("greeting"))));
    }
}

#[test]
fn settings_lists_keep_bottom_padding_for_remote_and_wheel_scrolling() {
    use crate::state::SETTINGS_LIST_BOTTOM_PADDING;
    let mut state = ready();
    state.preferences.reduced_motion = true;
    state.open(2);
    state.set_wallpapers(
        (1..=8)
            .map(|index| crate::wallpapers::Wallpaper {
                id: format!("custom-{index:032x}"),
                directory: std::path::PathBuf::from(".build/padding-fixture"),
                has_original: true,
            })
            .collect(),
    );
    state.section = 1;
    let last = Target::Background(state.background_count() - 1);
    state.focus = last.clone();
    state.reveal_focus(&controls(&state));
    let bottom =
        state.content_viewport().y + state.content_viewport().h - SETTINGS_LIST_BOTTOM_PADDING;
    let rect = controls(&state)
        .into_iter()
        .find(|c| c.target == last)
        .unwrap()
        .rect;
    assert!(rect.y + rect.h <= bottom + 0.01);
    state.wheel_scroll(&controls(&state), Some((1000., 600.)), 0, -1000);
    let rect = controls(&state)
        .into_iter()
        .find(|c| c.target == last)
        .unwrap()
        .rect;
    assert!((rect.y + rect.h - bottom).abs() < 0.01);
}

#[test]
fn apps_scroll_behind_navigation_without_a_top_cutoff_or_fade() {
    use crate::renderer::Draw;
    let mut state = ready();
    state.open(1);
    state.scroll = 400.;
    let rendered = scene(&state);
    assert!(!rendered.draw.iter().any(|draw| matches!(
        draw,
        Draw::Clip(Some(_)) | Draw::EdgeFade(Some(_)) | Draw::EdgeFadeX(Some(_))
    )));
    let card = rendered
        .draw
        .iter()
        .position(|draw| {
            matches!(draw,
                Draw::Shadow(rect, ..) if rect.y < 0. && rect.y + rect.h > 0.
            )
        })
        .expect("cards remain drawn until they leave the screen, not the content viewport");
    let nav_label = rendered
        .draw
        .iter()
        .rposition(|draw| {
            matches!(draw,
                Draw::Text(label, ..) if label == state.preferences.language.t("apps")
            )
        })
        .unwrap();
    assert!(card < nav_label);
    let nav = rendered
        .controls
        .iter()
        .find(|control| control.target == Target::Nav(1))
        .unwrap();
    let (x, y) = nav.rect.center();
    assert_eq!(
        state.hit_test(&rendered.controls, x, y),
        Some(Target::Nav(1))
    );
}

#[test]
fn apps_toolbar_stays_fixed_above_scrolling_cards_and_keeps_pointer_priority() {
    for with_clear_controls in [false, true] {
        let mut state = ready();
        state.open(1);
        if with_clear_controls {
            state.filter = 1;
            state.query = "n".into();
        }
        let before = controls(&state);
        state.scroll = 200.;
        let rendered = scene(&state);
        for control in before.iter().filter(|control| {
            matches!(
                control.target,
                Target::Filter(_) | Target::ClearFilter | Target::Search | Target::ClearSearch
            )
        }) {
            let fixed = rendered
                .controls
                .iter()
                .find(|item| item.target == control.target)
                .unwrap();
            assert_eq!(fixed.rect.y, control.rect.y);
            assert!(!fixed.scrollable);
            let (x, y) = fixed.rect.center();
            assert_eq!(
                state.hit_test(&rendered.controls, x, y),
                Some(fixed.target.clone())
            );
        }
        assert_eq!(state.hit_test(&rendered.controls, 1400., 230.), None);
        let first = before
            .iter()
            .find(|control| matches!(control.target, Target::App(_)))
            .unwrap();
        let scrolled = rendered
            .controls
            .iter()
            .find(|control| control.target == first.target)
            .unwrap();
        assert_eq!(first.rect.y - scrolled.rect.y, state.scroll);
        state.focus = first.target.clone();
        state.reveal_focus(&rendered.controls);
        let revealed = controls(&state)
            .into_iter()
            .find(|control| control.target == first.target)
            .unwrap();
        assert!(revealed.rect.y >= state.apps_viewport().y);
    }
}

#[test]
fn app_categories_and_search_use_matching_pills_and_larger_heavier_labels() {
    use crate::renderer::Draw;
    let mut state = ready();
    state.open(1);
    let rendered = scene(&state);
    for control in rendered
        .controls
        .iter()
        .filter(|control| matches!(control.target, Target::Filter(_) | Target::Search))
    {
        assert_eq!(control.rect.h, 56.);
        assert_eq!(control.rect.y, 164.8);
        assert!(rendered.draw.iter().any(|draw| matches!(draw,
            Draw::Shape(rect, radius, _, color) if rect.x == control.rect.x && rect.y == control.rect.y
                && rect.w == control.rect.w && (*radius == 48. || *radius == 28.) && color[3] >= 0.72
        )));
        assert!(rendered.draw.iter().any(|draw| matches!(draw,
            Draw::Text(_, rect, style, ..) if rect.x >= control.rect.x && rect.x < control.rect.x + control.rect.w
                && rect.y > control.rect.y && rect.y < control.rect.y + control.rect.h
                && style.size == 20.8 && style.weight == 550
        )));
    }
}

#[test]
fn category_focus_active_and_clear_button_share_the_pill_style() {
    use crate::renderer::Draw;
    let mut state = ready();
    state.open(1);
    state.filter = 1;
    state.focus = Target::ClearFilter;
    let rendered = scene(&state);
    let clear = rendered
        .controls
        .iter()
        .find(|control| control.target == Target::ClearFilter)
        .unwrap();
    assert_eq!((clear.rect.w, clear.rect.h), (56., 56.));
    assert!(rendered.draw.iter().any(|draw| matches!(draw,
        Draw::RoundedGradient(rect, radius, ..) if rect.x == clear.rect.x && rect.y == clear.rect.y && *radius == 28.
    )));
    let category = rendered
        .controls
        .iter()
        .find(|control| control.target == Target::Filter(1))
        .unwrap();
    assert!(rendered.draw.iter().any(|draw| matches!(draw,
        Draw::Shape(rect, radius, stroke, color) if rect.x == category.rect.x && rect.y == category.rect.y
            && *radius == 28. && *stroke == 1.5 && color[3] == 0.35
    )));
    state.focus = Target::Filter(1);
    let focused = scene(&state);
    assert!(focused.draw.iter().any(|draw| matches!(draw,
        Draw::RoundedGradient(rect, radius, ..) if rect.x == category.rect.x && rect.y == category.rect.y && *radius == 28.
    )));
}

#[test]
fn clock_date_and_home_input_descriptions_use_larger_text() {
    use crate::renderer::Draw;
    for language in crate::i18n::Language::ALL {
        let mut state = ready();
        state.preferences.language = language;
        let rendered = scene(&state);
        let now = time::OffsetDateTime::UNIX_EPOCH;
        for (label, size) in [
            (
                language.clock(
                    now,
                    state.preferences.hour_24,
                    state.preferences.show_seconds,
                ),
                24.,
            ),
            (language.date(now), 18.4),
        ] {
            assert!(rendered.draw.iter().any(|draw| matches!(draw,
                Draw::Text(text, rect, style, ..) if text == &label && style.size == size && rect.h >= size
            )));
        }
        let input = &state.inventory.inputs[0];
        let description = view::input_status(
            input,
            state.preferences.input_name(input).is_some(),
            language,
        );
        assert!(rendered.draw.iter().any(|draw| matches!(draw,
            Draw::Text(text, rect, style, ..) if text == &description && style.size == 16.8 && rect.h == 25.2
        )));
    }
}

#[test]
fn tv_settings_button_is_centered_alongside_navigation() {
    let rendered = scene(&ready());
    let button = rendered
        .controls
        .iter()
        .find(|control| control.target == Target::TvSettings)
        .unwrap();
    assert!((button.rect.y + button.rect.h / 2. - (42.5 + 68.6 / 2.)).abs() < 0.01);
    assert_eq!(button.rect.h, 60.);
    assert_eq!(button.rect.w, button.rect.h);
    assert_eq!(button.rect.x + button.rect.w, 1836.);
    let (x, y) = button.rect.center();
    assert_eq!(
        ready().hit_test(&rendered.controls, x, y),
        Some(Target::TvSettings)
    );
}

#[test]
fn greeting_visibility_toggle_persists_without_clearing_name_or_hiding_date() {
    use crate::{renderer::Draw, state::Subpage};
    let legacy: Preferences = serde_json::from_str(r#"{"greeting_name":"Ana"}"#).unwrap();
    assert!(legacy.show_greeting);
    let mut state = ready();
    state.preferences = legacy;
    state.open(2);
    state.focus = Target::Widget(0);
    state.activate();
    assert_eq!(state.subpage, Some(Subpage::Greeting));
    assert_eq!(state.focus, Target::GreetingToggle);
    let rendered = scene(&state);
    let toggle = rendered
        .controls
        .iter()
        .find(|control| control.target == Target::GreetingToggle)
        .unwrap();
    assert_eq!(toggle.rect.y, 164.8);
    assert!(rendered.draw.iter().any(|draw| matches!(draw,
        Draw::Text(label, ..) if label == state.preferences.language.t("showWidget")
    )));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(!state.preferences.show_greeting);
    assert_eq!(state.preferences.greeting_name, "Ana");
    let saved = serde_json::to_string(&state.preferences).unwrap();
    let restored: Preferences = serde_json::from_str(&saved).unwrap();
    assert!(!restored.show_greeting);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::GreetingName);
    state.focus = Target::GreetingToggle;
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::Widget(0));
    state.open(0);
    let hidden = scene(&state);
    let language = state.preferences.language;
    let now = time::OffsetDateTime::UNIX_EPOCH;
    let greeting = language.greeting(now.hour(), "Ana");
    assert!(
        !hidden
            .draw
            .iter()
            .any(|draw| matches!(draw, Draw::Text(label, ..) if label == &greeting))
    );
    assert!(
        hidden
            .draw
            .iter()
            .any(|draw| matches!(draw, Draw::Text(label, ..) if label == &language.date(now)))
    );
    state.open(2);
    state.focus = Target::Widget(0);
    state.activate();
    state.focus = Target::GreetingToggle;
    state.activate();
    assert!(state.preferences.show_greeting);
    state.open(0);
    assert!(
        scene(&state)
            .draw
            .iter()
            .any(|draw| matches!(draw, Draw::Text(label, ..) if label == &greeting))
    );
}

#[test]
fn date_widget_settings_persist_and_restore_parent_focus() {
    use crate::{model::DateFormat, state::Subpage};
    let legacy: Preferences = serde_json::from_str("{}").unwrap();
    assert!(legacy.show_date);
    assert_eq!(legacy.date_format, DateFormat::Localized);
    let mut state = ready();
    state.open(2);
    state.focus = Target::Widget(4);
    state.activate();
    assert_eq!(state.subpage, Some(Subpage::Date));
    assert_eq!(state.focus, Target::DateToggle);
    let before = controls(&state);
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert!(!state.preferences.show_date);
    let after = controls(&state);
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(a.target, b.target);
        assert_eq!((a.rect.x, a.rect.y), (b.rect.x, b.rect.y));
    }
    state.direction(Direction::Down, &after);
    assert_eq!(state.focus, Target::DateFormat);
    state.direction(Direction::Right, &controls(&state));
    assert_eq!(state.subpage, Some(Subpage::DateFormat));
    assert_eq!(state.focus, Target::Choice(0));
    for index in 1..4 {
        state.direction(Direction::Down, &controls(&state));
        assert_eq!(state.focus, Target::Choice(index));
    }
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::Choice(0));
    for _ in 0..3 {
        state.direction(Direction::Down, &controls(&state));
    }
    assert_eq!(state.focus, Target::Choice(3));
    assert_eq!(state.activate(), vec![Effect::Save]);
    assert_eq!(state.preferences.date_format, DateFormat::YearMonthDay);
    assert_eq!(state.subpage, Some(Subpage::Date));
    assert_eq!(state.focus, Target::DateFormat);
    state.activate();
    assert_eq!(state.focus, Target::Choice(3));
    state.back();
    assert_eq!(state.focus, Target::DateFormat);
    state.back();
    assert_eq!(state.focus, Target::Widget(4));
    let saved: Preferences =
        serde_json::from_str(&serde_json::to_string(&state.preferences).unwrap()).unwrap();
    assert!(!saved.show_date);
    assert_eq!(saved.date_format, DateFormat::YearMonthDay);
    state.open(0);
    let date = saved
        .date_format
        .format(saved.language, time::OffsetDateTime::UNIX_EPOCH);
    assert!(
        !scene(&state)
            .draw
            .iter()
            .any(|draw| matches!(draw, crate::renderer::Draw::Text(label, ..) if label == &date))
    );
}

#[test]
fn date_formats_preserve_localized_default_and_pad_numeric_dates() {
    use crate::{i18n::Language, model::DateFormat};
    let now = time::Date::from_calendar_date(2026, time::Month::February, 3)
        .unwrap()
        .midnight()
        .assume_utc();
    for language in Language::ALL {
        assert_eq!(
            DateFormat::Localized.format(language, now),
            language.date(now)
        );
        assert_eq!(DateFormat::DayMonthYear.format(language, now), "03.02.2026");
        assert_eq!(DateFormat::MonthDayYear.format(language, now), "02/03/2026");
        assert_eq!(DateFormat::YearMonthDay.format(language, now), "2026-02-03");
        for key in ["date", "dateFormat", "dateLocalized"] {
            assert_ne!(language.t(key), key);
        }
    }
}

#[test]
fn update_card_shows_both_versions_and_explicitly_starts_installation() {
    use crate::{i18n::Language, renderer::Draw};
    for language in Language::ALL {
        let mut state = ready();
        state.preferences.language = language;
        state.open(2);
        state.section = crate::state::SYSTEM_SECTION;
        assert!(
            !controls(&state)
                .iter()
                .any(|control| control.target == Target::InstallUpdate)
        );
        state.available_update = Some("9.0.0".into());
        let rendered = scene(&state);
        for label in [
            language.t("updateAvailable").to_owned(),
            language.format("installUpdate", &[("version", "9.0.0")]),
            language.format(
                "updateVersions",
                &[("current", "test"), ("latest", "9.0.0")],
            ),
        ] {
            assert!(
                rendered
                    .draw
                    .iter()
                    .any(|draw| matches!(draw, Draw::Text(text, ..) if text == &label))
            );
        }
        state.focus = Target::Sidebar(crate::state::SYSTEM_SECTION);
        state.direction(Direction::Right, &rendered.controls);
        assert_eq!(state.focus, Target::InstallUpdate);
        assert_eq!(
            state.activate(),
            vec![Effect::UpdateDownload("9.0.0".into())]
        );
        let button = rendered
            .controls
            .iter()
            .find(|control| control.target == Target::InstallUpdate)
            .unwrap();
        let (x, y) = button.rect.center();
        assert_eq!(
            state.hit_test(&rendered.controls, x, y),
            Some(Target::InstallUpdate)
        );
        state.direction(Direction::Down, &rendered.controls);
        assert_eq!(state.focus, Target::Setting(3));
        state.focus = Target::InstallUpdate;
        state.direction(Direction::Left, &rendered.controls);
        assert_eq!(state.focus, Target::Sidebar(crate::state::SYSTEM_SECTION));
    }
}

#[test]
fn update_status_card_only_claims_latest_after_success_and_allows_single_flight_retry() {
    use crate::{renderer::Draw, update::Status};
    let mut state = ready();
    state.open(2);
    state.section = crate::state::SYSTEM_SECTION;
    state.focus = Target::CheckUpdates;
    assert_eq!(state.update_status, Status::Unchecked);
    assert!(!scene(&state).draw.iter().any(|draw| matches!(draw,
        Draw::Text(label, ..) if label == state.preferences.language.t("latestVersionInstalled")
    )));
    assert!(state.activate().is_empty());
    assert_eq!(state.update_status, Status::Checking);
    assert!(std::mem::take(&mut state.update_check_requested));
    state.activate();
    assert!(!state.update_check_requested);
    state.update_response(Err("offline".into()));
    assert_eq!(state.update_status, Status::Failed);
    assert!(!scene(&state).draw.iter().any(|draw| matches!(draw,
        Draw::Text(label, ..) if label == state.preferences.language.t("latestVersionInstalled")
    )));
    state.activate();
    assert!(std::mem::take(&mut state.update_check_requested));
    state.update_response(Ok(None));
    assert_eq!(state.update_status, Status::Checked);
    for key in ["latestVersionInstalled", "checkAgain"] {
        assert!(scene(&state).draw.iter().any(|draw| matches!(draw,
            Draw::Text(label, ..) if label == state.preferences.language.t(key)
        )));
    }
    state.activate();
    state.update_response(Ok(Some("9.0.0".into())));
    assert_eq!(state.focus, Target::InstallUpdate);
    state.update_response(Err("offline".into()));
    assert_eq!(state.available_update.as_deref(), Some("9.0.0"));
}

#[test]
fn update_card_shows_progress_restores_retry_and_ignores_presses_while_busy() {
    use crate::update::{InstallStatus, Response, Status};
    let mut state = ready();
    state.open(2);
    state.section = crate::state::SYSTEM_SECTION;
    state.focus = Target::CheckUpdates;
    assert!(state.activate().is_empty());
    assert_eq!(state.update_status, Status::Checking);
    assert!(
        state.activate().is_empty(),
        "no second check while one runs"
    );
    state.update_response(Ok(Some("9.9.9".into())));
    assert!(scene(&state).draw.iter().any(|draw| {
        matches!(draw, crate::renderer::Draw::Text(text, ..) if text == "Install 9.9.9")
    }));
    let initial = scene(&state);
    assert!(
        !initial
            .controls
            .iter()
            .any(|control| control.target == Target::Setting(8))
    );
    assert_eq!(
        state.activate(),
        vec![Effect::UpdateDownload("9.9.9".into())]
    );
    assert_eq!(state.update, InstallStatus::Downloading(0));
    assert!(state.activate().is_empty());
    state.install_response(Response::Progress(45));
    assert!(scene(&state).draw.iter().any(|draw| {
        matches!(draw, crate::renderer::Draw::Text(text, ..) if text == "Downloading 45 %")
    }));
    let geometry = |items: Vec<crate::state::Control>| {
        items
            .into_iter()
            .map(|item| {
                (
                    item.target,
                    item.rect.x,
                    item.rect.y,
                    item.rect.w,
                    item.rect.h,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        geometry(controls(&state)),
        geometry(initial.controls),
        "progress must not move hit targets"
    );
    state.install_response(Response::Installing("9.9.9".into()));
    assert!(state.activate().is_empty());
    state.pressed = Some(Target::InstallUpdate);
    state.install_response(Response::Finished(Err("disk full".into())));
    assert_eq!(state.focus, Target::CheckUpdates);
    assert!(state.pressed.is_none());
    assert!(state.available_update.is_none());
    assert!(!state.update_busy());
    state.activate();
    assert_eq!(state.update_status, Status::Checking);
    assert_eq!(state.update, InstallStatus::Idle);
    state.install_response(Response::Finished(Ok(())));
    assert_eq!(state.update_status, Status::Unchecked);
}

#[test]
fn astros_registered_input_is_listed_once_after_the_hdmi_ports() {
    let inputs = normalize_inputs(&json!([
        {"appId":"com.danielcanoh22.astro", "id":"MVPD_IP-com.danielcanoh22.astro", "label":"Astro", "port":1},
        {"appId":"com.danielcanoh22.astro", "id":"MVPD_IP-com.danielcanoh22.astro", "label":"Astro"},
        {"appId":"com.danielcanoh22.astro", "id":"ASTRO", "label":"Astro"},
        {"appId":"com.webos.app.hdmi1", "id":"HDMI_1", "port":1}
    ]))
    .unwrap();
    let ids: Vec<_> = inputs.iter().map(|input| input.id.as_str()).collect();
    assert_eq!(ids, ["com.danielcanoh22.astro", "com.webos.app.hdmi1"]);
    assert_eq!(inputs[0].title, "Astro");
    assert_eq!(inputs[0].port, "Astro");
    assert_eq!(inputs[0].connected, Some(true));
    assert_eq!(inputs[0].signal, None);
    // Without a registration the row only has the HDMI ports.
    let inputs = normalize_inputs(&json!([
        {"appId":"com.webos.app.hdmi1", "id":"HDMI_1", "port":1}
    ]))
    .unwrap();
    assert_eq!(inputs.len(), 1);
}

#[test]
fn tv_input_row_reads_once_toggles_and_rereads_after_a_failed_change() {
    use crate::state::{TV_INPUT_ROW, TvInput};
    let mut state = ready();
    assert_eq!(state.tv_input_due(), None);
    state.open(2);
    state.section = crate::state::SYSTEM_SECTION;
    assert_eq!(state.tv_input_due(), Some(Effect::TvInput(None)));
    assert_eq!(state.tv_input_due(), None);
    assert!(
        controls(&state)
            .iter()
            .any(|c| c.target == Target::Setting(TV_INPUT_ROW))
    );
    state.focus = Target::Setting(TV_INPUT_ROW);
    assert_eq!(state.activate(), vec![]);
    assert!(state.tv_input_response(None, Ok(false)).is_empty());
    assert_eq!(state.activate(), vec![Effect::TvInput(Some(true))]);
    assert_eq!(state.tv_input, TvInput::Busy);
    assert_eq!(
        state.tv_input_response(Some(true), Ok(true)),
        vec![Effect::Refresh]
    );
    assert_eq!(state.tv_input, TvInput::Known(true));
    assert_eq!(state.activate(), vec![Effect::TvInput(Some(false))]);
    assert_eq!(
        state.tv_input_response(Some(false), Err("denied".into())),
        vec![Effect::TvInput(None)]
    );
    assert!(state.notice.is_some());
    assert!(
        state
            .tv_input_response(None, Err("no root".into()))
            .is_empty()
    );
    assert_eq!(state.tv_input, TvInput::Unavailable);
    assert_eq!(state.activate(), vec![Effect::TvInput(Some(true))]);
}

#[cfg(all(feature = "webos", unix))]
#[test]
fn the_home_log_never_writes_through_a_planted_link() {
    let scratch = std::env::temp_dir().join(format!("astro-home-log-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let plain = crate::open_home_log(&scratch.join("plain.log")).is_ok();
    // A second start replaces the previous log.
    let again = crate::open_home_log(&scratch.join("plain.log")).is_ok();
    std::fs::write(scratch.join("target"), b"keep").unwrap();
    std::os::unix::fs::symlink(scratch.join("target"), scratch.join("linked.log")).unwrap();
    let linked = crate::open_home_log(&scratch.join("linked.log")).is_ok();
    let replaced_link = !scratch.join("linked.log").is_symlink();
    std::fs::hard_link(scratch.join("target"), scratch.join("shared.log")).unwrap();
    let shared = crate::open_home_log(&scratch.join("shared.log")).is_ok();
    let target = std::fs::read(scratch.join("target")).unwrap();
    std::fs::remove_dir_all(&scratch).unwrap();
    assert!(plain && again);
    // A planted link is replaced by a fresh file; what it pointed to is never written.
    assert!(linked && replaced_link && shared);
    assert_eq!(target, b"keep");
}

#[test]
fn a_wallpaper_cross_fade_is_one_background_layer_group_with_its_folded_fills() {
    use crate::renderer::{Background, Draw, Scrim, TextStyle, background};
    use crate::state::Rect;
    let mut state = ready();
    state.preferences.background = "dunes".into();
    state.previous_background = Some(("forest".into(), std::time::Instant::now()));
    let draw = scene(&state).draw;
    let layers = background(&draw).expect("a cross-fade is drawn as one background pass");
    assert!(layers.wallpaper.ends_with("wallpapers/forest.jpg"));
    let (incoming, alpha) = layers
        .incoming
        .expect("the incoming wallpaper joins the pass");
    assert!(incoming.ends_with("wallpapers/dunes.jpg"));
    assert!((0. ..=1.).contains(&alpha));
    assert!(layers.scrims.is_empty());
    assert_eq!(layers.len, 2);

    // Recents shown during a fade: its full-screen backdrop folds into the same pass.
    state.open_recents();
    state.recents.visibility = 1.;
    let draw = scene(&state).draw;
    let layers = background(&draw).unwrap();
    assert!(layers.incoming.is_some());
    assert!(!layers.scrims.is_empty());
    assert!(
        !matches!(&draw[layers.len], Draw::Shape(rect, radius, _, _) if *radius <= 0. && rect.w >= 1920. && rect.h >= 1080.),
        "no full-screen fill directly above the wallpapers is left as a separate pass",
    );

    let screen = Rect::new(0., 0., 1920., 1080.);
    let fill = Draw::Shape(screen, 0., 0., [0.1, 0.2, 0.3, 0.5]);
    let gradient = Draw::RoundedGradient(screen, 0., [0., 0., 0., 0.2], [0., 0., 0., 0.8]);
    let commands = [
        Draw::Image("a.jpg".into(), screen, 0., false),
        Draw::TintedImage("b.jpg".into(), screen, 0., false, [1., 1., 1., 0.25]),
        fill.clone(),
        gradient,
        Draw::Text("x".into(), screen, TextStyle::new(20.), [1.; 4], false),
    ];
    assert_eq!(
        background(&commands),
        Some(Background {
            wallpaper: "a.jpg",
            incoming: Some(("b.jpg", 0.25)),
            scrims: vec![
                Scrim::Flat([0.1, 0.2, 0.3, 0.5]),
                Scrim::Gradient([0., 0., 0., 0.2], [0., 0., 0., 0.8]),
            ],
            len: 4,
        })
    );
    // Only a full-screen square image tinted in alpha alone is an incoming wallpaper.
    for layer in [
        Draw::TintedImage("b.jpg".into(), screen, 0., false, [0.85, 0.85, 0.85, 0.5]),
        Draw::TintedImage("b.jpg".into(), screen, 12., false, [1., 1., 1., 0.5]),
        Draw::TintedImage("b.jpg".into(), screen, 0., true, [1., 1., 1., 0.5]),
        Draw::TintedImage(
            "b.jpg".into(),
            Rect::new(0., 0., 960., 1080.),
            0.,
            false,
            [1., 1., 1., 0.5],
        ),
    ] {
        let commands = [commands[0].clone(), layer, commands[4].clone()];
        assert_eq!(background(&commands), None);
    }
}

#[test]
fn a_static_wallpaper_keeps_its_folded_fills_and_a_lone_wallpaper_its_own_pass() {
    use crate::renderer::{Background, Draw, Scrim, TextStyle, background};
    use crate::state::Rect;
    let screen = Rect::new(0., 0., 1920., 1080.);
    let wallpaper = Draw::Image("a.jpg".into(), screen, 0., false);
    let fill = Draw::Shape(screen, 0., 0., [0.1, 0.2, 0.3, 0.7]);
    let text = Draw::Text("x".into(), screen, TextStyle::new(20.), [1.; 4], false);
    assert_eq!(
        background(&[wallpaper.clone(), fill, text.clone()]),
        Some(Background {
            wallpaper: "a.jpg",
            incoming: None,
            scrims: vec![Scrim::Flat([0.1, 0.2, 0.3, 0.7])],
            len: 2,
        })
    );
    assert_eq!(background(&[wallpaper, text.clone()]), None);
    assert_eq!(background(&[text]), None);
    // A static scene starts with its plain wallpaper, drawn alone.
    let draw = scene(&ready()).draw;
    assert!(matches!(&draw[0], Draw::Image(path, ..) if path.ends_with("wallpapers/dunes.jpg")));
    assert_eq!(background(&draw), None);
}
