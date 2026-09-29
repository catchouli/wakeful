//! The fullscreen transition stage: a black-cover post-process that sits
//! between the UI and the dither.
//!
//! Consumers (the battle's entry/exit, scene teleports later) drive a
//! `TransitionState` resource through its modes; this module owns the
//! camera, the shader, and applying the state to it every frame. The
//! stage renders *before* the dither so transitions get the same retro
//! treatment as everything else.

use bevy::camera::RenderTarget;
use bevy::core_pipeline::fullscreen_material::FullscreenMaterial;
use bevy::core_pipeline::tonemapping::tonemapping;
use bevy::core_pipeline::{Core2d, Core2dSystems};
use bevy::ecs::schedule::{IntoScheduleConfigs, ScheduleConfigs, ScheduleLabel};
use bevy::ecs::system::BoxedSystem;
use bevy::prelude::*;
use bevy::render::extract_component::ExtractComponent;
use bevy::render::render_resource::ShaderType;
use bevy::shader::ShaderRef;

/// Seconds for a full black rise or fall. Shared with the battle's
/// phase machine so the curtain and the cover move together.
pub(crate) const COVER_SECS: f32 = 1.5;

/// Which way the black cover is moving, if at all.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub(crate) enum TransitionMode {
    #[default]
    Idle,
    /// The black cover rises: progress runs 0 → 1.
    BlackRise,
    /// The black cover falls: progress runs 1 → 0, then back to Idle.
    BlackFall,
}

/// The transition's current state. Consumers set the mode and let this
/// module animate the progress and apply it to the post-process.
#[derive(Resource, Default)]
pub(crate) struct TransitionState {
    pub(crate) mode: TransitionMode,
    pub(crate) progress: f32,
}

/// The post-process settings: the shader mixes the game image toward
/// black by `progress` when the mode is active.
#[derive(Component, ExtractComponent, Clone, Copy, ShaderType, Default, PartialEq)]
pub(crate) struct TransitionPostProcess {
    pub(crate) mode: u32,
    pub(crate) progress: f32,
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

/// Spawns the transition camera: a fullscreen pass that draws into the
/// game image after the UI and before the dither.
pub(crate) fn spawn_transition_camera(commands: &mut Commands, game_image: &Handle<Image>) {
    commands.spawn((
        Camera2d,
        Camera {
            order: crate::screen::TRANSITION_ORDER,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        Msaa::Off,
        TransitionPostProcess::default(),
        RenderTarget::Image(game_image.clone().into()),
    ));
}

/// Animates the transition's progress and applies the state to the
/// post-process settings every frame.
pub(crate) fn drive_transition(
    mut state: ResMut<TransitionState>,
    time: Res<Time<Fixed>>,
    mut settings: Query<&mut TransitionPostProcess>,
) {
    let dt = time.delta().as_secs_f32();
    match state.mode {
        TransitionMode::BlackRise => {
            state.progress = (state.progress + dt / COVER_SECS).min(1.0);
        }
        TransitionMode::BlackFall => {
            state.progress = (state.progress - dt / COVER_SECS).max(0.0);
            if state.progress <= 0.0 {
                state.mode = TransitionMode::Idle;
            }
        }
        TransitionMode::Idle => {}
    }
    let mode = match state.mode {
        TransitionMode::Idle => 0u32,
        TransitionMode::BlackRise => 1u32,
        TransitionMode::BlackFall => 2u32,
    };
    for mut settings in &mut settings {
        settings.mode = mode;
        settings.progress = state.progress;
    }
}
