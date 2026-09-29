//! The battle framework: staging, turn sequencing, and the script
//! surface that builds battles.
//!
//! The engine owns almost nothing about a battle's *content*: a
//! participant is an identity, a staged transform, a `time_until_act`
//! value, and an opaque data bag (HP/MP are conventions the scripts
//! keep in the bag, not engine fields). Every participant has a
//! **brain** — a script answering "your turn: what do you do?" — and
//! actions are named handlers registered by the battle script. The
//! same machinery drives menu-driven heroes (the shipped player brain)
//! and AI enemies, so the engine never distinguishes sides.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::SystemParam;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::state::state::NextState;
use rhai::{Dynamic, Engine, Scope};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::Player;
use crate::game_state::GameState;
use crate::input::InputManager;
use crate::systems::animation::Locomotion;
use crate::systems::camera::BattleCamera;
use crate::world_state::WorldState;

use crate::systems::actor::ActorModel;
use crate::systems::animation::EmoteRequest;
use crate::systems::bubble::screen_to_world;
use crate::systems::ui::{UiApi, UiRequest};

/// The generated arena's height (tools/generate_arena.py). The arena
/// cube sits centered on its node, so staging lifts it by half: its
/// floor lands on the origin and the fight happens at field level.
const ARENA_HEIGHT: f32 = 10.0;

/// How long the frozen-frame swirl and the fade home each take.
const SWIRL_SECS: f32 = 1.5;
/// A stalled field capture can't hold the battle hostage.
const FREEZE_TIMEOUT_SECS: f32 = 2.0;

// ---------------------------------------------------------------- script state

/// A brain reference: a script file loaded per participant, or a named
/// function in the battle script's own AST (inline brains coordinate
/// bespoke fights).
pub(crate) enum Brain {
    File {
        script: crate::scripts::ActorScript,
    },
    Inline {
        source: crate::scripts::ScriptSource,
        fn_name: String,
    },
}

/// A named action handler: a function in the battle script that
/// registered it; called with the actor's id, returns the recovery
/// time.
#[derive(Clone)]
pub(crate) struct ActionHandler {
    pub(crate) source: crate::scripts::ScriptSource,
    pub(crate) fn_name: String,
}

/// A participant as the battle script defines it.
#[derive(Clone)]
pub(crate) struct ParticipantDef {
    pub(crate) id: String,
    /// This participant IS the field player: no model is staged; the
    /// player entity moves to the formation slot and fights from there.
    pub(crate) player: bool,
    pub(crate) model: String,
    pub(crate) position: Vec3,
    pub(crate) facing_degrees: f32,
    pub(crate) brain: Option<String>,
    pub(crate) time_until_act: f32,
    pub(crate) bag: rhai::Map,
}

/// `start_battle` arguments, verbatim from the script.
#[derive(Clone)]
pub(crate) struct StartBattle {
    pub(crate) arena: String,
    /// Camera position and look-at target, offsets from the arena
    /// origin (the engine adds the stage offset).
    pub(crate) camera_pos: Vec3,
    pub(crate) camera_look: Vec3,
    pub(crate) participants: Vec<ParticipantDef>,
}

/// Everything the scripts queued before the state transition fired:
/// the battle definition plus the actions and brains registered for
/// it. Consumed by `OnEnter(Battle)`.
#[derive(Resource, Default)]
pub(crate) struct PendingBattleStart {
    pub(crate) def: Option<StartBattle>,
    pub(crate) actions: BTreeMap<String, ActionHandler>,
    pub(crate) brains: Vec<(String, Brain)>,
}

/// What a script asked the battle engine to do.
pub(crate) enum BattleRequest {
    Start(StartBattle),
    /// The result string is for the script's own use after return.
    End {
        result: String,
    },
    /// The field-frame capture completed: show it as the frozen quad.
    Frozen {
        texture: Handle<Image>,
    },
    Action {
        name: String,
        handler: ActionHandler,
    },
    Brain {
        participant: String,
        brain: Brain,
    },
    Emote {
        participant: String,
        clip: String,
    },
}

/// A participant as scripts see it: engine fields plus the bag merged
/// in. Brains and the UI read this.
pub(crate) fn participant_map(
    id: &str,
    position: Vec3,
    time_until_act: f32,
    bag: &rhai::Map,
) -> rhai::Map {
    let mut map = rhai::Map::new();
    map.insert("id".into(), Dynamic::from(id.to_string()));
    map.insert("x".into(), Dynamic::from(position.x as f64));
    map.insert("y".into(), Dynamic::from(position.y as f64));
    map.insert("z".into(), Dynamic::from(position.z as f64));
    map.insert(
        "time_until_act".into(),
        Dynamic::from(time_until_act as f64),
    );
    for (key, value) in bag {
        map.insert(key.clone(), value.clone());
    }
    map
}

/// The script-facing half of the battle framework, shared with every
/// compiled script engine through host functions.
#[derive(Clone, Default, Resource)]
pub struct BattleHandle(Arc<Mutex<ScriptState>>);

#[derive(Default)]
struct ScriptState {
    active: bool,
    store: BTreeMap<String, Dynamic>,
    requests: Vec<BattleRequest>,
    camera: Option<CameraSnapshot>,
    /// All live participants, republished every running tick for
    /// `battle_participants()` readers.
    participants: Vec<rhai::Map>,
}

/// The battle camera as scripts see it, published every tick.
#[derive(Clone, Copy)]
pub(crate) struct CameraSnapshot {
    pub(crate) position: Vec3,
    /// Yaw and pitch in radians, bevy convention (look direction from
    /// yaw around Y then pitch).
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) fov_radians: f32,
}

impl BattleHandle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScriptState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, request: BattleRequest) {
        self.lock().requests.push(request);
    }

    pub(crate) fn take_requests(&self) -> Vec<BattleRequest> {
        std::mem::take(&mut self.lock().requests)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn pending_requests(&self) -> usize {
        self.lock().requests.len()
    }

    pub(crate) fn set_active(&self, active: bool) {
        self.lock().active = active;
    }

    pub(crate) fn active(&self) -> bool {
        self.lock().active
    }

    pub(crate) fn set_store(&self, key: &str, value: Dynamic) {
        self.lock().store.insert(key.to_owned(), value);
    }

    pub(crate) fn get_store(&self, key: &str) -> Dynamic {
        self.lock().store.get(key).cloned().unwrap_or(Dynamic::UNIT)
    }

    pub(crate) fn clear_store(&self) {
        self.lock().store.clear();
    }

    pub(crate) fn set_camera(&self, camera: CameraSnapshot) {
        self.lock().camera = Some(camera);
    }

    pub(crate) fn camera(&self) -> Option<CameraSnapshot> {
        self.lock().camera
    }

    pub(crate) fn publish_participants(&self, participants: Vec<rhai::Map>) {
        self.lock().participants = participants;
    }

    pub(crate) fn participants(&self) -> Vec<rhai::Map> {
        self.lock().participants.clone()
    }
}

/// Projects a battle-world point into virtual-screen pixels
/// (320x240, origin top-left) using the published battle camera.
/// Returns the on-screen position and whether the point faces the
/// camera at all.
pub(crate) fn project_to_screen(point: Vec3, camera: &CameraSnapshot) -> (Vec2, bool) {
    // View-space transform: translate then rotate by the inverse of
    // the camera's yaw/pitch.
    let d = point - camera.position;
    let (yaw_sin, yaw_cos) = camera.yaw.sin_cos();
    let (pitch_sin, pitch_cos) = camera.pitch.sin_cos();
    // bevy cameras look down -Z: forward = R_y(yaw) * R_x(pitch) * -Z.
    let forward = Vec3::new(-yaw_sin * pitch_cos, pitch_sin, -yaw_cos * pitch_cos);
    let right = Vec3::new(yaw_cos, 0.0, -yaw_sin);
    let up = right.cross(forward);
    let x = d.dot(right);
    let y = d.dot(up);
    let depth = d.dot(forward);
    if depth <= 0.0 {
        return (Vec2::ZERO, false);
    }
    // Perspective: the projection plane sits 1 unit ahead; fov is
    // vertical.
    let half_fov = (camera.fov_radians / 2.0).tan();
    let aspect = 320.0 / 240.0;
    let ndc_x = (x / (depth * half_fov * aspect)).clamp(-1.5, 1.5);
    let ndc_y = (y / (depth * half_fov)).clamp(-1.5, 1.5);
    let screen_x = (ndc_x * 0.5 + 0.5) * 320.0;
    let screen_y = (0.5 - ndc_y * 0.5) * 240.0;
    (Vec2::new(screen_x, screen_y), true)
}

// ---------------------------------------------------------------- engine state

/// Phases: capture the field frame, swirl it away (zoom + fade — the
/// camera cut happens behind the opaque peak), reveal the fight, then
/// the mirror-image fade home. The field camera never visibly moves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Freezing,
    Swirl { elapsed: f32 },
    Reveal { elapsed: f32 },
    Running,
    Returning { elapsed: f32 },
}

/// One staged combatant on the engine side.
pub(crate) struct Combatant {
    pub(crate) id: String,
    pub(crate) entity: Entity,
    pub(crate) position: Vec3,
    pub(crate) time_until_act: f32,
    pub(crate) bag: rhai::Map,
    pub(crate) brain: Option<Brain>,
}

/// The engine-side battle, present as a resource only while one runs.
#[derive(Resource)]
pub(crate) struct Battle {
    pub(crate) phase: Phase,
    pub(crate) arena: Entity,
    pub(crate) participants: Vec<Combatant>,
    /// Action handlers by name.
    pub(crate) actions: BTreeMap<String, ActionHandler>,
    pub(crate) result: Option<String>,
    /// The Freezing phase fires the capture exactly once.
    pub(crate) capture_requested: bool,
    /// How long the Freezing phase has waited for its capture.
    pub(crate) freezing_elapsed: f32,
    /// The field player's pose before the battle, restored on return.
    pub(crate) player_return: Option<(Vec3, Quat)>,
    /// The player participant's entity (the field player itself).
    pub(crate) player_entity: Option<Entity>,
}

impl Battle {
    /// The participant with the lowest time-until-act, if any.
    pub(crate) fn next_actor(&self) -> Option<usize> {
        self.participants
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.time_until_act.total_cmp(&b.1.time_until_act))
            .map(|(i, _)| i)
    }

    /// Whether the battle's own 3D view should render right now: not
    /// during the capture (the scene view persists under the frozen
    /// frame, so the swap hides behind a picture of itself), and not
    /// after the exit fade's opaque peak (the scene view comes back
    /// under the fading black).
    pub(crate) fn battle_view_active(&self) -> bool {
        match &self.phase {
            Phase::Freezing => false,
            Phase::Swirl { .. } | Phase::Reveal { .. } | Phase::Running => true,
            Phase::Returning { elapsed } => *elapsed < SWIRL_SECS / 2.0,
        }
    }
}

/// The frozen-frame curtain's material: textureless (black) between
/// battles, holding the captured scene picture during the swirl.
#[derive(Resource)]
pub(crate) struct FrozenMaterial(pub(crate) Handle<ColorMaterial>);

/// The frozen field frame shown during the swirl.
#[derive(Component)]
pub(crate) struct BattleFrozen;

/// Marks a battle participant entity (model root + brain + bag live in
/// `Battle`).
#[derive(Component)]
pub(crate) struct BattleParticipant;

// ---------------------------------------------------------------- registration

/// Wires battle host functions into a script engine. `battle` carries
/// script-visible state; `engine`/`ast` are the *calling* script's,
/// captured so inline brains and actions can be invoked later.
pub(crate) fn register_battle_api(
    engine: &mut Engine,
    battle: &BattleHandle,
    source: &crate::scripts::ScriptSource,
) {
    use crate::scripts::runtime_error;

    {
        let battle = battle.clone();
        engine.register_fn("in_battle", move || -> bool { battle.active() });
    }

    {
        let battle = battle.clone();
        engine.register_fn(
            "start_battle",
            move |arena: &str,
                  cam: rhai::Map,
                  participants: rhai::Array|
                  -> Result<(), Box<rhai::EvalAltResult>> {
                let camera_pos = vec_from_map(&cam, "pos")?;
                let camera_look = vec_from_map(&cam, "look")?;
                let mut defs = Vec::new();
                for entry in &participants {
                    let map = entry.clone().try_cast::<rhai::Map>().ok_or_else(|| {
                        runtime_error("start_battle: participants must be maps".into())
                    })?;
                    let id = map
                        .get("id")
                        .and_then(|v| v.clone().into_string().ok())
                        .ok_or_else(|| {
                            runtime_error("start_battle: participant needs id".into())
                        })?;
                    let player = map
                        .get("player")
                        .and_then(|v| v.as_bool().ok())
                        .unwrap_or(false);
                    let model = map
                        .get("model")
                        .and_then(|v| v.clone().into_string().ok())
                        .unwrap_or_else(|| "models/character.glb".into());
                    let position = vec_from_map(&map, "position")?;
                    let facing = map
                        .get("facing")
                        .and_then(|v| v.as_float().ok())
                        .unwrap_or(0.0) as f32;
                    let brain = map.get("brain").and_then(|v| v.clone().into_string().ok());
                    let time_until_act = map
                        .get("time_until_act")
                        .and_then(|v| v.as_float().ok())
                        .unwrap_or(0.0) as f32;
                    let bag = map
                        .get("bag")
                        .and_then(|v| v.clone().try_cast::<rhai::Map>())
                        .unwrap_or_default();
                    defs.push(ParticipantDef {
                        id,
                        player,
                        model,
                        position,
                        facing_degrees: facing,
                        brain,
                        time_until_act,
                        bag,
                    });
                }
                battle.push(BattleRequest::Start(StartBattle {
                    arena: arena.to_owned(),
                    camera_pos,
                    camera_look,
                    participants: defs,
                }));
                Ok(())
            },
        );
    }

    {
        let battle = battle.clone();
        engine.register_fn("end_battle", move |result: &str| {
            battle.push(BattleRequest::End {
                result: result.to_owned(),
            });
        });
    }

    {
        let battle = battle.clone();
        let source = source.clone();
        engine.register_fn("battle_action", move |name: &str, fn_name: &str| {
            battle.push(BattleRequest::Action {
                name: name.to_owned(),
                handler: ActionHandler {
                    source: source.clone(),
                    fn_name: fn_name.to_owned(),
                },
            });
        });
    }

    {
        let battle = battle.clone();
        let source = source.clone();
        engine.register_fn("battle_brain", move |participant: &str, fn_name: &str| {
            battle.push(BattleRequest::Brain {
                participant: participant.to_owned(),
                brain: Brain::Inline {
                    source: source.clone(),
                    fn_name: fn_name.to_owned(),
                },
            });
        });
    }

    {
        let battle = battle.clone();
        engine.register_fn("battle_set", move |key: &str, value: Dynamic| {
            battle.set_store(key, value);
        });
    }

    {
        let battle = battle.clone();
        engine.register_fn("battle_get", move |key: &str| -> Dynamic {
            battle.get_store(key)
        });
    }

    {
        let battle = battle.clone();
        engine.register_fn("battle_emote", move |participant: &str, clip: &str| {
            battle.push(BattleRequest::Emote {
                participant: participant.to_owned(),
                clip: clip.to_owned(),
            });
        });
    }
}

/// Participants as maps for brains and UI: engine fields with the bag
/// merged over them. Registered as `battle_participants()`; needs the
/// live list, so the engine publishes it into the handle each tick.
pub(crate) fn register_battle_readers(engine: &mut Engine, battle: &BattleHandle) {
    use crate::scripts::runtime_error;

    {
        let battle = battle.clone();
        engine.register_fn("battle_participants", move || -> rhai::Array {
            battle
                .participants()
                .into_iter()
                .map(Dynamic::from)
                .collect()
        });
    }

    {
        let battle = battle.clone();
        engine.register_fn(
            "battle_screen_pos",
            move |x: f64, y: f64, z: f64| -> Result<rhai::Map, Box<rhai::EvalAltResult>> {
                let camera = battle
                    .camera()
                    .ok_or_else(|| runtime_error("battle_screen_pos: no battle camera".into()))?;
                let (screen, onscreen) =
                    project_to_screen(Vec3::new(x as f32, y as f32, z as f32), &camera);
                let mut map = rhai::Map::new();
                map.insert("x".into(), Dynamic::from(screen.x as f64));
                map.insert("y".into(), Dynamic::from(screen.y as f64));
                map.insert("onscreen".into(), Dynamic::from(onscreen));
                Ok(map)
            },
        );
    }

    {
        let battle = battle.clone();
        engine.register_fn(
            "battle_bag_set",
            move |participant: &str, key: &str, value: Dynamic| {
                battle
                    .lock()
                    .store
                    .insert(format!("bag:{participant}:{key}"), value);
            },
        );
    }

    {
        let battle = battle.clone();
        engine.register_fn(
            "battle_bag_get",
            move |participant: &str, key: &str| -> Dynamic {
                battle
                    .lock()
                    .store
                    .get(&format!("bag:{participant}:{key}"))
                    .cloned()
                    .unwrap_or(Dynamic::UNIT)
            },
        );
    }
}

fn vec_from_map(map: &rhai::Map, key: &str) -> Result<Vec3, Box<rhai::EvalAltResult>> {
    let value = map
        .get(key)
        .ok_or_else(|| crate::scripts::runtime_error(format!("missing '{key}'")))?;
    let array = value
        .clone()
        .try_cast::<rhai::Array>()
        .ok_or_else(|| crate::scripts::runtime_error(format!("'{key}' must be [x, y, z]")))?;
    let mut out = [0.0f64; 3];
    for (i, entry) in array.iter().take(3).enumerate() {
        out[i] = entry.as_float().unwrap_or(0.0);
    }
    Ok(Vec3::new(out[0] as f32, out[1] as f32, out[2] as f32))
}

/// The camera transform (position + yaw/pitch) derived from a position
/// and a look-at target.
pub(crate) fn camera_from_look(position: Vec3, look: Vec3) -> (Vec3, Vec2) {
    let dir = (look - position).normalize_or_zero();
    let yaw = (-dir.x).atan2(-dir.z);
    let pitch = dir.y.asin().clamp(-1.5, 1.5);
    (position, Vec2::new(yaw, pitch))
}

// ---------------------------------------------------------------- engine systems

/// Everything the battle systems need, gathered once.
/// The field player, disjoint from the battle camera (both want
/// `Transform`).
type PlayerTransforms<'w, 's> =
    Query<'w, 's, (Entity, &'static mut Transform), (With<Player>, Without<BattleCamera>)>;

#[allow(clippy::type_complexity)]
#[derive(SystemParam)]
pub(crate) struct BattleTurnParams<'w, 's> {
    pub(crate) battle: ResMut<'w, BattleHandle>,
    pub(crate) state: Option<ResMut<'w, Battle>>,
    pub(crate) next_state: ResMut<'w, NextState<GameState>>,
    pub(crate) camera:
        Query<'w, 's, (&'static mut Transform, &'static mut Projection), With<BattleCamera>>,
    pub(crate) frozen_quads: Query<
        'w,
        's,
        (&'static mut Transform, &'static mut Visibility),
        (With<BattleFrozen>, Without<BattleCamera>),
    >,
    pub(crate) transition: ResMut<'w, crate::transition::TransitionState>,
}

/// Drains script requests, runs phases, and sequences turns. Sits at
/// the head of FixedUpdate so `in_battle()` is fresh for every other
/// system and script this tick.
/// Drains script requests in BOTH states: before a battle they
/// accumulate into [`PendingBattleStart`] (whose `OnEnter` staging
/// consumes them); during one they apply to the live battle. Start and
/// End drive the state transition itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn battle_requests(
    mut commands: Commands,
    battle: ResMut<BattleHandle>,
    mut battle_state: Option<ResMut<Battle>>,
    mut next_state: ResMut<NextState<GameState>>,
    mut pending: ResMut<PendingBattleStart>,
    mut emote_targets: Query<&mut EmoteRequest>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    frozen_material: Res<FrozenMaterial>,
    mut frozen_visibilities: Query<
        &'static mut Visibility,
        (With<BattleFrozen>, Without<BattleCamera>),
    >,
) {
    let mut end: Option<String> = None;
    for request in battle.take_requests() {
        match request {
            BattleRequest::Start(def) => {
                if pending.def.is_some() {
                    warn!("start_battle ignored: a battle is already starting");
                }
                pending.def = Some(def);
                next_state.set(GameState::Battle);
            }
            BattleRequest::End { result } => end = Some(result),
            BattleRequest::Frozen { texture } => {
                if let Some(b) = battle_state.as_mut() {
                    // The boot-spawned curtain takes the captured
                    // picture and covers the screen at full opacity.
                    if let Some(mut material) = materials.get_mut(&frozen_material.0) {
                        material.texture = Some(texture);
                    }
                    for mut visibility in &mut frozen_visibilities {
                        *visibility = Visibility::Visible;
                    }
                    // The curtain is up: the fighters take the stage
                    // behind it.
                    for combatant in &b.participants {
                        commands
                            .entity(combatant.entity)
                            .insert(Visibility::Visible);
                    }
                    if let Some(player) = b.player_entity {
                        commands.entity(player).insert(Visibility::Visible);
                    }
                    b.phase = Phase::Swirl { elapsed: 0.0 };
                }
            }
            BattleRequest::Action { name, handler } => {
                if let Some(b) = battle_state.as_mut() {
                    b.actions.insert(name, handler);
                } else {
                    pending.actions.insert(name, handler);
                }
            }
            BattleRequest::Brain { participant, brain } => {
                if let Some(b) = battle_state.as_mut()
                    && let Some(c) = b.participants.iter_mut().find(|c| c.id == participant)
                {
                    c.brain = Some(brain);
                } else {
                    pending.brains.push((participant, brain));
                }
            }
            BattleRequest::Emote { participant, clip } => {
                if let Some(b) = battle_state.as_mut()
                    && let Some(c) = b.participants.iter().find(|c| c.id == participant)
                    && let Some(mut request) = emote_targets.get_mut(c.entity).ok()
                {
                    request.emote = Some(clip);
                }
            }
        }
    }
    if let Some(result) = end {
        if let Some(b) = battle_state.as_mut() {
            bevy::log::warn!(
                "battle: drain received end_battle('{result}'), phase={:?}",
                b.phase
            );
            // The Returning phase owns the exit: fade, restore, then the
            // state flip.
            b.result = Some(result);
            b.phase = Phase::Returning { elapsed: 0.0 };
        } else {
            bevy::log::warn!("battle: end_battle with no battle running");
        }
    }
}

/// The battle-internal machine: phases, the turn sequencer, the fade.
pub(crate) fn battle_turns(mut params: BattleTurnParams, time: Res<Time<Fixed>>) {
    let Some(state) = params.state.as_mut() else {
        return;
    };

    // The phase machine. No camera moves: each state owns its camera
    // and the activation sync swaps the views.
    let dt = time.delta().as_secs_f32();
    let mut finished = false;
    match state.phase {
        Phase::Freezing => {
            // Waiting for the capture system (Update schedule —
            // readbacks spawned from FixedUpdate never complete); the
            // Frozen request advances the phase. A stalled capture
            // can't hold the battle hostage.
            state.freezing_elapsed += dt;
            if state.freezing_elapsed > FREEZE_TIMEOUT_SECS {
                state.phase = Phase::Swirl { elapsed: 0.0 };
            }
        }
        Phase::Swirl { mut elapsed } => {
            elapsed += dt;
            // The frozen picture zooms gently while the black cover
            // rises over it; at full cover it drops away.
            let t = (elapsed / SWIRL_SECS).min(1.0);
            let zoom = 1.0 + 0.18 * ease(t);
            for (mut transform, _) in &mut params.frozen_quads {
                transform.scale = Vec3::new(340.0 * zoom, 260.0 * zoom, 1.0);
            }
            params.transition.mode = crate::transition::TransitionMode::BlackRise;
            params.transition.progress = t;
            if t >= 1.0 {
                for (_, mut visibility) in &mut params.frozen_quads {
                    *visibility = Visibility::Hidden;
                }
                state.phase = Phase::Reveal { elapsed: 0.0 };
            } else {
                state.phase = Phase::Swirl { elapsed };
            }
        }
        Phase::Reveal { mut elapsed } => {
            elapsed += dt;
            // The black cover falls away, revealing the live battle.
            let t = (elapsed / SWIRL_SECS).min(1.0);
            params.transition.mode = crate::transition::TransitionMode::BlackFall;
            params.transition.progress = 1.0 - t;
            if t >= 1.0 {
                params.transition.mode = crate::transition::TransitionMode::Idle;
                state.phase = Phase::Running;
            } else {
                state.phase = Phase::Reveal { elapsed };
            }
        }
        Phase::Returning { mut elapsed } => {
            elapsed += dt;
            // The black cover rises over the battle view; the peak
            // hands the state back to the scene.
            let t = (elapsed / SWIRL_SECS).min(1.0);
            params.transition.mode = crate::transition::TransitionMode::BlackRise;
            params.transition.progress = t;
            if t >= 1.0 {
                params.transition.mode = crate::transition::TransitionMode::BlackFall;
                finished = true;
            } else {
                state.phase = Phase::Returning { elapsed };
            }
        }
        Phase::Running => {
            sequence_turn(&params.battle, &mut params.camera, state);
        }
    }
    if finished {
        // The state machine owns the exit: dropping the flag lets the
        // OnExit cleanup (and every module's restore) run; the
        // transition's BlackFall then fades the scene up.
        params.next_state.set(GameState::Scene);
    }
}

/// Spawns the arena, the participants, and the fade logic; stores the
/// field camera for the return trip.
/// `OnEnter(Battle)` staging: builds the arena and the participants
/// from the pending start, wires brains (files + the inline ones the
/// battle script registered), and captures the field camera for the
/// return trip.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_battle<'w, 's>(
    mut commands: Commands,
    mut pending: ResMut<PendingBattleStart>,
    handle: ResMut<BattleHandle>,
    assets: Res<AssetServer>,
    mut players: PlayerTransforms<'w, 's>,
    mut camera: Query<&'static mut Transform, With<BattleCamera>>,
    input: Res<InputManager>,
    ui: Res<UiApi>,
    world_state: Res<WorldState>,
) {
    let Some(def) = pending.def.take() else {
        warn!("entered Battle without a pending start");
        return;
    };
    // The cube is centered on its node; lift it so its floor lands on
    // the origin height.
    let arena = commands
        .spawn((
            ActorModel(assets.load(&def.arena)),
            Visibility::default(),
            Transform::from_translation(Vec3::new(0.0, ARENA_HEIGHT / 2.0, 0.0)),
        ))
        .id();

    // Brain files compile with the full script environment: a menu
    // brain needs the UI, a coordinated pack needs the battle store.
    let env = crate::scripts::ScriptEnv::new(
        input.handle(),
        ui.clone(),
        world_state.clone(),
        handle.clone(),
    );
    let mut participants = Vec::new();
    let mut player_return = None;
    for def in &def.participants {
        let rotation = Quat::from_rotation_y(def.facing_degrees.to_radians());
        // The player participant IS the field player: the party
        // leader's model walks into the formation slot, and its whole
        // animation pipeline just works. Enemies stage exactly like
        // the arena does.
        let entity = if def.player {
            let Ok((entity, mut transform)) = players.single_mut() else {
                warn!("battle needs the field player; skipping {}", def.id);
                continue;
            };
            player_return = Some((transform.translation, transform.rotation));
            transform.translation = def.position;
            transform.rotation = rotation;
            // Hidden until the frozen frame covers the view swap.
            commands.entity(entity).insert(Visibility::Hidden);
            entity
        } else {
            let model = def.model.clone();
            commands
                .spawn((
                    BattleParticipant,
                    Locomotion::default(),
                    ActorModel(assets.load(&model)),
                    // Hidden until the frozen frame covers the view
                    // swap: the fighters take the stage behind it.
                    Visibility::Hidden,
                    Transform::from_translation(def.position).with_rotation(rotation),
                ))
                .id()
        };
        let mut brain = None;
        if let Some(path) = &def.brain {
            brain = crate::scripts::ActorScript::load(path, env.clone().with_store(path))
                .map(|script| Brain::File { script });
        }
        participants.push(Combatant {
            id: def.id.clone(),
            entity,
            position: def.position,
            time_until_act: def.time_until_act,
            bag: def.bag.clone(),
            brain,
        });
    }
    // The player combatant, for the return trip.
    let player_entity = participants
        .iter()
        .find(|c| {
            c.entity
                == players
                    .single()
                    .map(|(e, _)| e)
                    .unwrap_or(Entity::PLACEHOLDER)
        })
        .map(|c| c.entity);
    // The battle camera takes the script's pose; the scene camera is
    // never touched, so there is nothing to restore on the way out.
    if let Ok(mut transform) = camera.single_mut() {
        let (position, yaw_pitch) = camera_from_look(def.camera_pos, def.camera_look);
        transform.translation = position;
        let (yaw, pitch) = (yaw_pitch.x, yaw_pitch.y);
        transform.rotation = Quat::from_euler(EulerRot::YXZ, pitch, yaw, 0.0);
    }

    // The store starts clean, then is seeded with the starting bags:
    // handler scripts read and write bag values through it, and
    // sequence_turn folds the store back into the participants before
    // publishing.
    handle.set_active(true);
    handle.clear_store();
    for participant in &participants {
        for (key, value) in &participant.bag {
            let store_key = format!("bag:{}:{key}", participant.id);
            handle.set_store(&store_key, value.clone());
        }
    }
    commands.insert_resource(Battle {
        phase: Phase::Freezing,
        arena,
        participants,
        actions: std::mem::take(&mut pending.actions),
        result: None,
        capture_requested: false,
        freezing_elapsed: 0.0,
        player_entity,
        player_return,
    });
}

/// `OnExit(Battle)`: the arena and the fighters go, the frozen frame
/// goes, and the scene modules restore themselves in their own hooks.
pub(crate) fn cleanup_battle(
    mut commands: Commands,
    battle: Option<Res<Battle>>,
    handle: ResMut<BattleHandle>,
    ui: Res<UiApi>,
    mut transition: ResMut<crate::transition::TransitionState>,
) {
    let Some(battle) = battle else {
        return;
    };
    // The transition's BlackFall fades the scene up on the other side.
    transition.mode = crate::transition::TransitionMode::BlackFall;
    transition.progress = 1.0;
    // A battle script that errored can't run its own exit cleanup, so
    // the engine sweeps every battle window regardless.
    ui.push(UiRequest::Close {
        name: "battle".to_owned(),
    });
    ui.push(UiRequest::Close {
        name: "battle-status".to_owned(),
    });
    for n in 0..8 {
        ui.push(UiRequest::Close {
            name: format!("float{n}"),
        });
    }
    for combatant in &battle.participants {
        if Some(combatant.entity) == battle.player_entity {
            if let Some((translation, rotation)) = battle.player_return {
                commands
                    .entity(combatant.entity)
                    .insert(Transform::from_translation(translation).with_rotation(rotation));
            }
        } else {
            commands.entity(combatant.entity).despawn();
        }
    }
    commands.entity(battle.arena).despawn();
    commands.remove_resource::<Battle>();
    handle.set_active(false);
    handle.clear_store();
}

/// One running-battle tick: publish state for scripts, find who acts,
/// ask their brain, dispatch the chosen action.
fn sequence_turn(
    battle: &BattleHandle,
    camera: &mut Query<(&mut Transform, &mut Projection), With<BattleCamera>>,
    state: &mut Battle,
) {
    // Script-side bag mutations land in the store ("bag:<id>:<key>");
    // fold them into the participant bags before publishing, so the
    // HUD sees the damage the actions dealt.
    {
        let store = battle.lock();
        for participant in &mut state.participants {
            let prefix = format!("bag:{}:", participant.id);
            for (key, value) in store.store.range(prefix.clone()..) {
                if !key.starts_with(&prefix) {
                    break;
                }
                participant
                    .bag
                    .insert(key[prefix.len()..].into(), value.clone());
            }
        }
    }

    // Publish participants for `battle_participants()` readers.
    let maps: Vec<rhai::Map> = state
        .participants
        .iter()
        .map(|c| participant_map(&c.id, c.position, c.time_until_act, &c.bag))
        .collect();
    battle.publish_participants(maps);

    // Publish the camera snapshot for `battle_screen_pos`.
    if let Ok((transform, Projection::Perspective(perspective))) = camera.single() {
        let fov = perspective.fov;
        let (yaw, pitch, _) = transform.rotation.to_euler(EulerRot::YXZ);
        battle.set_camera(CameraSnapshot {
            position: transform.translation,
            yaw,
            pitch,
            fov_radians: fov,
        });
    }

    // Give every participant a little time back each tick? No: only
    // the active one consumes time — everyone else is frozen while the
    // current actor acts, which is what turn-based means here.
    let Some(index) = state.next_actor() else {
        return;
    };
    let (id, position, time_until_act, bag) = {
        let c = &state.participants[index];
        (c.id.clone(), c.position, c.time_until_act, c.bag.clone())
    };
    let Some(brain) = state.participants[index].brain.as_ref() else {
        warn!("battle: participant {id} has no brain; idling");
        state.participants[index].time_until_act += 1.0;
        return;
    };

    let participant = participant_map(&id, position, time_until_act, &bag);
    let decision = decide_with_brain(brain, participant);
    match decision {
        Ok(choice) => {
            let Some(action_name) = choice else {
                // Still deciding (a menu waiting for input): try again
                // next tick.
                return;
            };
            // A unit means the brain (usually the menu) is still
            // deciding; anything non-string is a script bug.
            if action_name.is_unit() {
                return;
            }
            let Ok(action_name) = action_name.into_string() else {
                warn!("battle: brain for {id} returned a non-string action");
                state.participants[index].time_until_act += 1.0;
                return;
            };
            let Some(handler) = state.actions.get(&action_name).cloned() else {
                warn!("battle: no action '{action_name}' registered");
                state.participants[index].time_until_act += 1.0;
                return;
            };
            let recovery = run_action(&handler, &id);
            state.participants[index].time_until_act += recovery;
        }
        Err(e) => {
            warn!("battle: brain for {id} errored: {e}");
            state.participants[index].time_until_act += 1.0;
        }
    }
}

fn decide_with_brain(brain: &Brain, participant: rhai::Map) -> Result<Option<Dynamic>, String> {
    match brain {
        Brain::File { script, .. } => {
            let mut scope = Scope::new();
            script
                .call_dynamic(&mut scope, "decide", participant.into())
                .map(Some)
                .map_err(|e| e.to_string())
        }
        Brain::Inline { source, fn_name } => {
            let inner = source.lock().unwrap_or_else(PoisonError::into_inner);
            let inner = inner.as_ref().ok_or("brain source went away")?;
            let mut scope = Scope::new();
            inner
                .engine
                .call_fn(&mut scope, &inner.ast, fn_name, (participant,))
                .map(Some)
                .map_err(|e| e.to_string())
        }
    }
}

fn run_action(handler: &ActionHandler, actor_id: &str) -> f32 {
    let inner = handler
        .source
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(inner) = inner.as_ref() else {
        warn!("battle: action '{}' lost its source", handler.fn_name);
        return 1.0;
    };
    let mut scope = Scope::new();
    match inner.engine.call_fn::<f64>(
        &mut scope,
        &inner.ast,
        &handler.fn_name,
        (actor_id.to_string(),),
    ) {
        Ok(recovery) => recovery as f32,
        Err(e) => {
            warn!("battle: action '{}' errored: {e}", handler.fn_name);
            1.0
        }
    }
}

/// Captures the live game image: a GPU readback whose observer hands
/// the frame back as a texture (see the Frozen request). Lives in the
/// Update schedule — readbacks spawned from FixedUpdate never
/// complete.
pub(crate) fn capture_frame(
    mut commands: Commands,
    game_image: Option<Res<crate::screen::GameImage>>,
    battle: Option<ResMut<Battle>>,
) {
    let (Some(game_image), Some(mut battle)) = (game_image, battle) else {
        return;
    };
    if battle.phase != Phase::Freezing || battle.capture_requested {
        return;
    }
    battle.capture_requested = true;
    commands
        .spawn(Readback::texture(game_image.0.clone()))
        .observe(
            move |trigger: On<ReadbackComplete>,
                  mut images: ResMut<Assets<Image>>,
                  mut commands: Commands,
                  handle: Res<BattleHandle>| {
                let expected =
                    (crate::screen::GAME_WIDTH * crate::screen::GAME_HEIGHT * 4) as usize;
                if trigger.data.len() != expected {
                    warn!(
                        "battle: field capture came back {} bytes",
                        trigger.data.len()
                    );
                    return;
                }
                let image = Image::new(
                    Extent3d {
                        width: crate::screen::GAME_WIDTH,
                        height: crate::screen::GAME_HEIGHT,
                        depth_or_array_layers: 1,
                    },
                    TextureDimension::D2,
                    trigger.data.clone(),
                    TextureFormat::Rgba8UnormSrgb,
                    RenderAssetUsages::MAIN_WORLD,
                );
                let texture = images.add(image);
                handle.push(BattleRequest::Frozen { texture });
                commands.entity(trigger.entity).remove::<Readback>();
            },
        );
}

fn ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------- fade overlay

/// Spawns the fullscreen fade quad once at startup; the battle engine
/// only re-alphas its material.
pub(crate) fn setup_fade(
    mut commands: Commands,
    mut materials: ResMut<Assets<ColorMaterial>>,
    bubbles: Res<crate::systems::bubble::BubbleAssets>,
) {
    // The frozen-frame curtain: a long-lived quad spawned at boot so its
    // visibility chain is established from the start — a quad spawned
    // mid-game via commands never got its InheritedVisibility computed
    // and stayed culled forever. It holds the captured scene picture
    // during the swirl; textureless and hidden between battles.
    let frozen_material = materials.add(ColorMaterial::default());
    commands.insert_resource(FrozenMaterial(frozen_material.clone()));
    let center = screen_to_world(Vec2::new(160.0, 120.0));
    commands.spawn((
        BattleFrozen,
        Mesh2d(bubbles.rect.clone()),
        MeshMaterial2d(frozen_material),
        Visibility::Hidden,
        Transform::from_translation(center.extend(1.4)).with_scale(Vec3::new(340.0, 260.0, 1.0)),
        RenderLayers::layer(crate::screen::UI_LAYER),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lowest_time_until_act_acts_first() {
        let battle = Battle {
            phase: Phase::Running,
            arena: Entity::PLACEHOLDER,
            participants: vec![
                Combatant {
                    id: "hero".into(),
                    entity: Entity::PLACEHOLDER,
                    position: Vec3::ZERO,
                    time_until_act: 1.2,
                    bag: rhai::Map::new(),
                    brain: None,
                },
                Combatant {
                    id: "goblin".into(),
                    entity: Entity::PLACEHOLDER,
                    position: Vec3::ZERO,
                    time_until_act: 0.3,
                    bag: rhai::Map::new(),
                    brain: None,
                },
            ],
            actions: BTreeMap::new(),
            result: None,
            capture_requested: false,
            freezing_elapsed: 0.0,
            player_entity: None,
            player_return: None,
        };
        assert_eq!(
            battle
                .next_actor()
                .map(|i| battle.participants[i].id.as_str()),
            Some("goblin")
        );
    }

    #[test]
    fn screen_positions_project_in_front_and_fail_behind() {
        // A camera at the stage's south edge looking north, the usual
        // battle shot: a point ahead projects near the screen middle;
        // a point behind the camera is reported off-screen.
        let camera = CameraSnapshot {
            position: Vec3::new(0.0, 3.0, 8.5),
            yaw: 0.0,
            pitch: -0.2,
            fov_radians: 45.0_f32.to_radians(),
        };
        let (front, onscreen) = project_to_screen(Vec3::new(0.0, 1.0, 0.0), &camera);
        assert!(onscreen);
        assert!((front.x - 160.0).abs() < 2.0, "centered: {front:?}");
        assert!(
            front.y > 100.0 && front.y < 140.0,
            "near the middle: {front:?}"
        );

        let (_, behind) = project_to_screen(Vec3::new(0.0, 1.0, 20.0), &camera);
        assert!(!behind);
    }

    #[test]
    fn the_shipped_player_brain_menus_then_commits() {
        use crate::input::InputState;
        use crate::scripts::{ActorScript, ScriptEnv};
        use crate::systems::ui::UiApi;

        let ui = UiApi::new();
        let env = ScriptEnv::new(
            crate::input::detached(),
            ui.clone(),
            crate::world_state::WorldState::default(),
            BattleHandle::new(),
        );
        let brain = ActorScript::compile_with_handle(
            include_str!("../assets/scripts/battle/player_brain.rhai"),
            env,
        )
        .expect("the shipped player brain must compile");

        let participant: rhai::Map = [
            ("id", Dynamic::from("hero".to_owned())),
            ("hp", Dynamic::from(87.0)),
            ("max_hp", Dynamic::from(100.0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();

        // No menu pick yet: still deciding.
        let mut scope = Scope::new();
        let choice = brain
            .call_dynamic(&mut scope, "decide", participant.clone().into())
            .unwrap();
        assert!(choice.is_unit());

        // The menu declared two options; pressing cross picks Attack.
        let nav = ui.nav().lock().unwrap_or_else(PoisonError::into_inner);
        let mut nav = nav;
        nav.declare("battle", 2);
        let mut input = InputState::default();
        input.inject(&[], &[crate::input::PadButton::Cross], &[]);
        nav.navigate(&input);
        drop(nav);

        let mut scope = Scope::new();
        let choice = brain
            .call_dynamic(&mut scope, "decide", participant.into())
            .unwrap();
        assert_eq!(choice.into_string().unwrap(), "attack");
    }

    #[test]
    fn an_inline_brain_answers_through_its_source_cell() {
        let engine = Engine::new();
        let ast = engine
            .compile("fn goblin_decide(participant) { \"attack\" }")
            .unwrap();
        let source: crate::scripts::ScriptSource =
            Arc::new(Mutex::new(Some(Arc::new(crate::scripts::ScriptInner {
                engine,
                ast,
            }))));
        let brain = Brain::Inline {
            source,
            fn_name: "goblin_decide".into(),
        };
        let participant = participant_map("goblin", Vec3::ZERO, 0.0, &rhai::Map::new());
        let choice = decide_with_brain(&brain, participant).unwrap().unwrap();
        assert_eq!(choice.into_string().unwrap(), "attack");
    }
}
