mod movement;

use bevy::image::ImagePlugin;
use bevy::prelude::*;

use crate::movement::{PLAYER_SPEED, move_position};

/// Scales the 16px source sprite up on screen.
const SPRITE_SCALE: f32 = 4.0;

#[derive(Component)]
struct Player;

fn main() {
    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "wakeful".into(),
                        ..default()
                    }),
                    ..default()
                })
                .set(ImagePlugin::default_nearest()),
        )
        .insert_resource(ClearColor(Color::srgb(0.10, 0.08, 0.13)))
        .add_systems(Startup, (setup_camera, spawn_player))
        .add_systems(Update, move_player)
        .run();
}

fn setup_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn spawn_player(mut commands: Commands, assets: Res<AssetServer>) {
    commands.spawn((
        Player,
        Sprite::from_image(assets.load("sprites/player.png")),
        Transform::from_scale(Vec3::splat(SPRITE_SCALE)),
    ));
}

fn move_player(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut players: Query<&mut Transform, With<Player>>,
) {
    let Ok(mut transform) = players.single_mut() else {
        return;
    };

    let mut direction = Vec2::ZERO;
    if keys.pressed(KeyCode::ArrowUp) {
        direction.y += 1.0;
    }
    if keys.pressed(KeyCode::ArrowDown) {
        direction.y -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowLeft) {
        direction.x -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowRight) {
        direction.x += 1.0;
    }

    transform.translation = move_position(
        transform.translation.truncate(),
        direction,
        PLAYER_SPEED,
        time.delta_secs(),
    )
    .extend(0.0);
}
