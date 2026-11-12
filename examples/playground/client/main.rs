//! The playground's client: connects to the server, predicts its own player and draws everyone.
//!
//! - the library's transport client holds the session, and `connect` adds the link
//!   conditioner's loss model and the window title;
//! - `presentation` places the players, the platform and the gate, and `demon` draws the players;
//! - `hit_ghosts` shows the pistol's lag-compensated hits, `rocket_visuals` the rockets;
//! - `scene`, `dev_texture` and `text` are the world's look, `camera` and `hud` the view;
//! - `autopilot` plays by itself, for the network tests.

use avian3d::prelude::*;
use bevy::{
    app::RunFixedMainLoop,
    input::{common_conditions::input_just_pressed, mouse::AccumulatedMouseMotion},
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PresentMode},
    winit::{UpdateMode::Continuous, WinitSettings},
};
use bevy_ahoy::prelude::*;
use bevy_egui::{EguiPlugin, input::egui_wants_any_pointer_input};
use bevy_enhanced_input::prelude::EnhancedInputPlugin;
use bevy_link_conditioner::{LinkConditionerPlugin, LinkConditionerUiPlugin};
use bevy_netahoy::{transport::NetAhoyTransportClientPlugin, *};
use bevy_replicon::prelude::*;

mod autopilot;
mod camera;
mod connect;
mod demon;
mod dev_texture;
mod hit_ghosts;
mod hud;
#[path = "../link_report.rs"]
mod link_report;
mod presentation;
mod rocket_visuals;
mod scene;
mod text;
use playground::{Arsenal, EQUIP_PISTOL, EQUIP_ROCKETS, Weapon, shared::*, time_scale_from_args};

fn main() -> AppExit {
    let time_scale = time_scale_from_args();
    let remote_ghost_debug = presentation::remote_ghost_debug_from_args();
    let demo = DemoArgs::from_args();

    let mut app = App::new();
    app.insert_resource(ClientLook::default())
        .insert_resource(remote_ghost_debug)
        .insert_resource(time_scale)
        .insert_resource(WinitSettings {
            focused_mode: Continuous,
            unfocused_mode: Continuous,
        });

    app.add_plugins((
        DefaultPlugins.set(window_plugin()),
        PhysicsPlugins::default(),
        EnhancedInputPlugin,
        AhoyPlugins::new(KccSchedule),
        ExampleSharedPlugin,
        NetAhoyClientPlugin::<Arsenal>::default(),
        ClientPlugin { demo },
        // `--net=<preset|spec>` conditions both directions of this client's
        // link; F2 tunes it live.
        EguiPlugin::default(),
        LinkConditionerPlugin::from_args(),
        LinkConditionerUiPlugin::default(),
        autopilot::AutopilotPlugin,
        text::TextPlugin,
        scene::ScenePlugin,
        demon::DemonPlugin,
        hit_ghosts::HitGhostsPlugin,
    ))
    .add_systems(Update, link_report::report_link_conditions);
    #[cfg(target_family = "wasm")]
    app.add_systems(First, vsync_clock.before(bevy::time::TimeSystems));
    if headless() {
        // No window to vsync against, so winit's runner would spin flat out:
        // a later runner wins, pinned to 60 Hz like a real client.
        app.add_plugins(bevy::app::ScheduleRunnerPlugin::run_loop(
            std::time::Duration::from_secs_f64(1.0 / client_loop_hz()),
        ));
    }
    app.run()
}

/// `--headless` or `HEADLESS=1`: no window and a fixed-rate loop. Rendering stays, so the
/// client runs as shipped; systems that need the window skip, and `AUTOPILOT=1` plays.
#[cfg(not(target_family = "wasm"))]
fn headless() -> bool {
    std::env::args().skip(1).any(|arg| arg == "--headless")
        || std::env::var("HEADLESS").is_ok_and(|value| value == "1" || value == "true")
}

#[cfg(target_family = "wasm")]
fn headless() -> bool {
    false
}

/// Bevy's clock read at the frame's vsync (`document.timeline`), not whenever the update starts:
/// each frame's delta is then what the screen advanced, so everything moved by it moves evenly,
/// the remote players drawn at the remote render time first. Measured on the judder rig at
/// 144 Hz, the flying bot's drawn path: 0.2 mm rough with it; 3.2 mm with Bevy's clock read
/// within 0.8 ms of vsync, as on a real display, and 35 mm on the rig's own late timer.
#[cfg(target_family = "wasm")]
fn vsync_clock(
    mut strategy: ResMut<bevy::time::TimeUpdateStrategy>,
    mut last: Local<Option<bevy::platform::time::Instant>>,
) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let vsync = window
        .document()
        .and_then(|document| document.timeline().current_time());
    let (Some(performance), Some(vsync)) = (window.performance(), vsync) else {
        return;
    };
    // How long ago this frame's vsync was, on the clock Bevy's Instant reads.
    let age = std::time::Duration::from_secs_f64((performance.now() - vsync).max(0.0) / 1000.0);
    let now = bevy::platform::time::Instant::now();
    let vsync = now.checked_sub(age).unwrap_or(now);
    // Two updates in one frame read the same vsync: a zero delta, never a negative one.
    let vsync = last.map_or(vsync, |last| vsync.max(last));
    *last = Some(vsync);
    *strategy = bevy::time::TimeUpdateStrategy::ManualInstant(vsync);
}

fn window_plugin() -> WindowPlugin {
    if headless() {
        return WindowPlugin {
            primary_window: None,
            exit_condition: bevy::window::ExitCondition::DontExit,
            ..default()
        };
    }
    WindowPlugin {
        primary_window: Some(Window {
            title: "bevy_netahoy playground".to_string(),
            resolution: (1280, 720).into(),
            present_mode: PresentMode::AutoVsync,
            ..default()
        }),
        ..default()
    }
}

#[derive(Default)]
struct DemoArgs {
    record: Option<std::path::PathBuf>,
    play: Option<std::path::PathBuf>,
}

impl DemoArgs {
    fn from_args() -> Self {
        Self {
            record: path_arg("--record"),
            play: path_arg("--play"),
        }
    }
}

fn path_arg(flag: &str) -> Option<std::path::PathBuf> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next().map(Into::into);
        }
    }
    None
}

#[derive(Resource, Default)]
struct ClientLook {
    yaw: f32,
    pitch: f32,
}

struct ClientPlugin {
    demo: DemoArgs,
}

impl Plugin for ClientPlugin {
    fn build(&self, app: &mut App) {
        rocket_visuals::add_client_rockets(app);
        playground::totals::add_shot_totals(app);

        if let Some(path) = &self.demo.play {
            // Demo playback is the network backend: no transport, no session.
            app.add_plugins(NetAhoyDemoPlaybackPlugin::file(path.clone()))
                .add_systems(
                    Update,
                    demo_playback_controls.run_if(resource_exists::<DemoPlayback>),
                );
        } else {
            // `SERVER_URL`, `WT_HASH_URL` and `TRANSPORT`, or the page's address and its
            // `?transport=`. It connects at startup, and reconnects whenever the link dies or
            // goes silent.
            app.add_plugins(NetAhoyTransportClientPlugin::from_settings())
                .add_observer(connect::condition_new_session)
                .add_systems(Update, connect::follow_link_mode);
            if let Some(path) = &self.demo.record {
                app.add_plugins(NetAhoyDemoRecordPlugin { path: path.clone() });
            }
        }

        app.add_systems(Startup, hud::setup_hud)
            .add_systems(
                Update,
                (
                    // A click on the F2 panel is for the panel.
                    capture_cursor.run_if(
                        input_just_pressed(MouseButton::Left)
                            .and_then(not(egui_wants_any_pointer_input)),
                    ),
                    release_cursor.run_if(input_just_pressed(KeyCode::Escape)),
                    update_client_look,
                )
                    .chain(),
            )
            .add_systems(
                RunFixedMainLoop,
                gather_client_input.in_set(NetAhoyClientSystems::GatherInput),
            )
            // Right after replication applies, before avian first sees the
            // body: its transform sync starts from the pose given here.
            .add_systems(
                PreUpdate,
                presentation::attach_platform.after(ClientSystems::Receive),
            )
            .add_systems(
                Update,
                (
                    presentation::attach_player_meshes,
                    presentation::sync_debug_ghosts_from_snapshots,
                    presentation::spawn_predicted_player,
                    presentation::spawn_remote_visuals,
                    presentation::attach_name_tags,
                    presentation::update_name_tags,
                    presentation::toggle_remote_ghost_debug,
                    presentation::update_local_presentation,
                    presentation::place_platform,
                    presentation::attach_gate,
                    presentation::place_gate,
                    camera::update_camera_from_local_presentation,
                    hud::update_status,
                    connect::title_window,
                )
                    .chain()
                    .after(NetAhoyClientSystems::Interpolate),
            );
    }
}

fn capture_cursor(mut cursor: Single<&mut CursorOptions>) {
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
}

fn release_cursor(mut cursor: Single<&mut CursorOptions>) {
    cursor.visible = true;
    cursor.grab_mode = CursorGrabMode::None;
}

fn update_client_look(
    cursor: Single<&CursorOptions>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    mut look: ResMut<ClientLook>,
) {
    if cursor.grab_mode != CursorGrabMode::Locked {
        return;
    }

    const MOUSE_SENSITIVITY: f32 = 0.07_f32.to_radians();
    look.yaw -= mouse_motion.delta.x * MOUSE_SENSITIVITY;
    look.pitch = (look.pitch - mouse_motion.delta.y * MOUSE_SENSITIVITY).clamp(-1.5, 1.5);
}

fn gather_client_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    look: Res<ClientLook>,
    fixed_time: Res<Time<Fixed>>,
    mut input: ResMut<ClientInput>,
    // The pistol to start with: the range is straight ahead.
    mut weapon: Local<Option<Weapon>>,
) {
    let mut movement = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        movement.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        movement.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        movement.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        movement.x += 1.0;
    }

    input.movement = movement;
    input.look = Vec2::new(look.yaw, look.pitch);
    let mut buttons = Buttons::empty();
    buttons.set(Buttons::JUMP, keys.pressed(KeyCode::Space));
    buttons.set(
        Buttons::CROUCH,
        keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::KeyC),
    );
    // Every command holds the pick's bit, so a lost command can't lose a switch.
    if keys.just_pressed(KeyCode::Digit1) {
        *weapon = Some(Weapon::Rockets);
    } else if keys.just_pressed(KeyCode::Digit2) {
        *weapon = Some(Weapon::Pistol);
    }
    buttons |= match weapon.unwrap_or(Weapon::Pistol) {
        Weapon::Rockets => EQUIP_ROCKETS,
        Weapon::Pistol => EQUIP_PISTOL,
    };
    input.buttons = buttons;

    // Press edges, accumulated per render frame so a tap between ticks still
    // lands on the next command.
    let mut pressed = Buttons::empty();
    pressed.set(Buttons::JUMP, keys.just_pressed(KeyCode::Space));
    input.pressed |= pressed;

    // The click keeps its look and its place in the tick; the library puts it in one command.
    if mouse.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::KeyF) {
        input.fire = Some(SubtickFire {
            frac: fixed_time.overstep_fraction(),
            look: Vec2::new(look.yaw, look.pitch),
            origin: None,
        });
    }
}

fn demo_playback_controls(keys: Res<ButtonInput<KeyCode>>, mut playback: ResMut<DemoPlayback>) {
    if keys.just_pressed(KeyCode::Space) {
        playback.speed = if playback.speed == 0.0 { 1.0 } else { 0.0 };
    }
    if keys.just_pressed(KeyCode::ArrowUp) {
        playback.speed = (playback.speed * 2.0).clamp(0.25, 8.0);
    }
    if keys.just_pressed(KeyCode::ArrowDown) {
        playback.speed = (playback.speed * 0.5).clamp(0.25, 8.0);
    }
}

/// The headless client's loop rate, `CLIENT_LOOP_HZ` (default 60, a common
/// display). Each loop flushes the client's packets, so this is also how
/// often it can send (and piggyback TCP ACKs).
fn client_loop_hz() -> f64 {
    std::env::var("CLIENT_LOOP_HZ")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|hz| *hz > 0.0)
        .unwrap_or(60.0)
}
