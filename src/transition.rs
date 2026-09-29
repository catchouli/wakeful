//! The transition choreographer: the state between contexts.
//!
//! When a script starts a battle (or, later, a teleporter warps), the
//! game enters the Transition state: the world freezes, the scene is
//! captured, the frozen picture zooms under a rising black cover, the
//! new context stages itself behind the opaque screen, and the cover
//! falls onto the live result. The state machine enforces the ordering
//! — the capture happens before any battle content exists, so the
//! curtain can only ever show the "from" view.
//!
//! Consumers register their own systems gated on the transition's
//! covered stage (the battle's staging and cleanup, scene warps later);
//! this module owns the stages, the capture, the curtain quad, the
//! cover shader, and the game-state flips.

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::core_pipeline::fullscreen_material::FullscreenMaterial;
use bevy::core_pipeline::tonemapping::tonemapping;
use bevy::core_pipeline::{Core2d, Core2dSystems};
use bevy::ecs::schedule::{IntoScheduleConfigs, ScheduleConfigs, ScheduleLabel};
use bevy::ecs::system::BoxedSystem;
use bevy::prelude::*;
use bevy::render::extract_component::ExtractComponent;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{Extent3d, ShaderType, TextureDimension, TextureFormat};
use bevy::shader::ShaderRef;

use crate::systems::bubble::screen_to_world;

use crate::screen::TRANSITION_ORDER;

/// Seconds for a full black rise or fall.
pub(crate) const COVER_SECS: f32 = 1.5;

/// Which context the transition is carrying the game between.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub(crate) enum TransitionKind {
    #[default]
    Battle,
    /// Scene warps, once teleport.rs drives this state.
    #[allow(dead_code)]
    Warp,
}

/// The choreography's stages, in order:
///
/// - `Capture`: the readback queues; the world is still the "from" view.
/// - `Curtain`: the picture is up; the cover rises over it.
/// - `Covered`: the cover is opaque — the consumer hooks fire (the
///   battle staging on the way in, the cleanup on the way out) and the
///   game state flips.
/// - `Reveal`: the cover falls onto the "to" view.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub(crate) enum TransitionStage {
    #[default]
    Idle,
    Capture,
    Curtain,
    Covered,
    Reveal,
}

/// Whether the transition is heading into a new context (the battle
/// staging fires at the cover) or back to the scene (the cleanup does).
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub(crate) enum TransitionDirection {
    #[default]
    Entering,
    Returning,
}

/// The transition's current state, driven by consumers calling
/// [`TransitionState::begin`] and animated by [`drive_transition`].
#[derive(Resource, Default)]
pub(crate) struct TransitionState {
    pub(crate) kind: TransitionKind,
    pub(crate) stage: TransitionStage,
    pub(crate) direction: TransitionDirection,
    /// 0 = clear, 1 = fully black.
    pub(crate) cover: f32,
}

impl TransitionState {
    /// Starts a transition toward a context (`entering`) or back to the
    /// scene (`returning`).
    pub(crate) fn begin(&mut self, kind: TransitionKind, direction: TransitionDirection) {
        self.kind = kind;
        self.direction = direction;
        // Returning needs no capture: the black simply rises over the
        // live view and the consumer's cleanup runs at the cover.
        self.stage = match direction {
            TransitionDirection::Entering => TransitionStage::Capture,
            TransitionDirection::Returning => TransitionStage::Curtain,
        };
        self.cover = 0.0;
    }

    /// Whether the battle's own 3D view should render: the staging has
    /// happened behind the cover, so the world is the battle's until
    /// the scene returns.
    pub(crate) fn battle_view(&self) -> bool {
        self.stage == TransitionStage::Covered || self.stage == TransitionStage::Reveal
    }
}

/// The transition stage's settings: the shader mixes the game image
/// toward black by `cover` while mode 1 (the swirl) magnifies it about
/// the screen center.
#[derive(Component, ExtractComponent, Clone, Copy, ShaderType, Default, PartialEq)]
pub(crate) struct TransitionPostProcess {
    pub(crate) mode: u32,
    pub(crate) cover: f32,
}

impl FullscreenMaterial for TransitionPostProcess {
    fn fragment_shader() -> ShaderRef {
        "shaders/transition.wgsl".into()
    }

    // The game image is assembled by Camera2d views, so the pass must
    // run in the 2d graph — the default Core3d never touches 2d views.
    fn schedule() -> impl ScheduleLabel + Clone {
        Core2d
    }

    fn schedule_configs(system: ScheduleConfigs<BoxedSystem>) -> ScheduleConfigs<BoxedSystem> {
        system
            .in_set(Core2dSystems::PostProcess)
            .before(tonemapping)
    }
}

/// The frozen-picture curtain's material: textureless (black) between
/// transitions, holding the captured scene picture during the swirl.
#[derive(Resource)]
pub(crate) struct FrozenMaterial(pub(crate) Handle<ColorMaterial>);

/// Spawns the transition camera and the curtain quad once at startup.
/// The camera's pass draws into the game image after the UI and before
/// the final post-processing; the curtain is a long-lived quad spawned
/// at boot so its visibility chain is established from the start.
pub(crate) fn setup(
    mut commands: Commands,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut images: ResMut<Assets<Image>>,
    game_image: Res<crate::screen::GameImage>,
    bubbles: Res<crate::systems::bubble::BubbleAssets>,
) {
    commands.spawn((
        Camera2d,
        Camera {
            order: TRANSITION_ORDER,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Msaa::Off,
        TransitionPostProcess::default(),
        RenderTarget::Image(game_image.0.clone().into()),
    ));

    // A 1x1 black placeholder so the curtain material always has a
    // texture; the capture swaps it.
    let placeholder = images.add(Image::new_fill(
        Extent3d::default(),
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD,
    ));
    let frozen_material = materials.add(ColorMaterial {
        texture: Some(placeholder),
        ..default()
    });
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

/// The frozen-picture curtain. Spawns once at boot; visibility and
/// texture are the choreography's to drive.
#[derive(Component)]
pub(crate) struct BattleFrozen;

/// Stage 1: queues the capture readback while the "from" view is still
/// fully on screen. The observer hands the frame back as a texture and
/// raises the curtain.
pub(crate) fn capture(
    mut commands: Commands,
    game_image: Res<crate::screen::GameImage>,
    mut state: ResMut<TransitionState>,
    _materials: Res<Assets<ColorMaterial>>,
    _frozen_material: Res<FrozenMaterial>,
) {
    if state.stage != TransitionStage::Capture {
        return;
    }
    state.stage = TransitionStage::Curtain;
    commands
        .spawn(Readback::texture(game_image.0.clone()))
        .observe(
            move |trigger: On<ReadbackComplete>,
                  mut images: ResMut<Assets<Image>>,
                  mut commands: Commands,
                  mut materials: ResMut<Assets<ColorMaterial>>,
                  frozen_material: Res<FrozenMaterial>,
                  mut curtain: Query<&mut Visibility, With<BattleFrozen>>| {
                let expected =
                    (crate::screen::GAME_WIDTH * crate::screen::GAME_HEIGHT * 4) as usize;
                if trigger.data.len() != expected {
                    warn!("transition: capture came back {} bytes", trigger.data.len());
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
                if let Some(mut material) = materials.get_mut(&frozen_material.0) {
                    material.texture = Some(texture);
                }
                // The picture is up: the boot-spawned curtain covers
                // the screen.
                for mut visibility in &mut curtain {
                    *visibility = Visibility::Visible;
                }
                commands.entity(trigger.entity).remove::<Readback>();
            },
        );
}

/// Stage 2+: animates the cover, toggles the curtain, and advances the
/// stages. The covered stage flips the game state; the consumer hooks
/// (staging, cleanup) are systems gated on the covered stage.
pub(crate) fn drive_transition(
    mut state: ResMut<TransitionState>,
    time: Res<Time<Fixed>>,
    mut settings: Query<&mut TransitionPostProcess>,
    mut curtain: Query<&mut Visibility, With<BattleFrozen>>,
    mut next_state: ResMut<NextState<crate::game_state::GameState>>,
) {
    let dt = time.delta().as_secs_f32();
    match state.stage {
        TransitionStage::Idle => {}
        TransitionStage::Capture => {}
        TransitionStage::Curtain => {
            state.cover = (state.cover + dt / COVER_SECS).min(1.0);
            if state.cover >= 1.0 {
                state.stage = TransitionStage::Covered;
                // The world is now the new context's, and the cover is
                // opaque: flip the game state, drop the curtain.
                if state.direction == TransitionDirection::Entering {
                    next_state.set(crate::game_state::GameState::Battle);
                } else {
                    next_state.set(crate::game_state::GameState::Scene);
                }
            }
        }
        TransitionStage::Covered => {
            // The consumer hooks fire this frame (gated on the covered
            // stage); the cover starts falling onto the live result.
            state.stage = TransitionStage::Reveal;
            for mut visibility in &mut curtain {
                *visibility = Visibility::Hidden;
            }
        }
        TransitionStage::Reveal => {
            state.cover = (state.cover - dt / COVER_SECS).max(0.0);
            if state.cover <= 0.0 {
                state.stage = TransitionStage::Idle;
            }
        }
    }
    let mode = match state.stage {
        TransitionStage::Idle => 0u32,
        _ => 1u32,
    };
    for mut settings in &mut settings {
        settings.mode = mode;
        settings.cover = state.cover;
    }
}

/// The run condition for consumer hooks that fire at the covered point:
/// the battle's staging (entering) and cleanup (returning), the scene's
/// suspension and resume.
pub(crate) fn at_covered(
    state: Res<State<crate::game_state::GameState>>,
    transition: Res<TransitionState>,
) -> bool {
    *state.get() == crate::game_state::GameState::Transition
        && transition.stage == TransitionStage::Covered
}

pub(crate) fn entering(transition: Res<TransitionState>) -> bool {
    transition.direction == TransitionDirection::Entering
}

pub(crate) fn returning(transition: Res<TransitionState>) -> bool {
    transition.direction == TransitionDirection::Returning
}
