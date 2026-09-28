//! The game's top-level state: playing a scene, or fighting a battle.
//!
//! Systems declare which state they belong to with `run_if(in_state(..))`
//! — the world freezes around a battle without any ad-hoc checks — and
//! each module suspends and restores its own entities in `OnEnter` /
//! `OnExit` hooks. Battle.rs and scene.rs share this vocabulary and
//! nothing else.

use bevy::prelude::*;

#[derive(States, Clone, PartialEq, Eq, Debug, Hash, Default)]
pub(crate) enum GameState {
    #[default]
    Scene,
    Battle,
}
