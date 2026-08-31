mod movement;
mod scene;
mod screen;

use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;
use bevy::window::WindowResolution;
use bevy::world_serialization::WorldAsset;
use bevy_common_assets::ron::RonAssetPlugin;

use crate::movement::{PLAYER_SPEED, move_position};
use crate::scene::Scene;

/// Movement logic runs on a fixed step so behavior doesn't depend on
/// display refresh rate or frame timing jitter.
const FIXED_HZ: f64 = 60.0;

/// Camera layer that draws the pre-rendered background image.
const BG_LAYER: usize = 2;

/// Capsule geometry; `PLAYER_Y` keeps it resting on the ground plane.
const PLAYER_RADIUS: f32 = 0.4;
const PLAYER_HALF_HEIGHT: f32 = 0.5;
const PLAYER_Y: f32 = PLAYER_RADIUS + PLAYER_HALF_HEIGHT;
const GROUND_SIZE: f32 = 30.0;
const PLAYER_COLOR: Color = Color::srgb(0.949, 0.651, 0.306);
const GROUND_COLOR: Color = Color::srgb(0.23, 0.21, 0.28);

#[derive(Component)]
struct Player;

#[derive(Component)]
struct GameCamera;

#[derive(Resource)]
struct CurrentScene(Handle<Scene>);

#[derive(Resource)]
struct SceneApplied(bool);

#[derive(Resource)]
struct PlayerModel(Handle<WorldAsset>);

type GameCameraQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Transform, &'static mut Projection),
    (With<GameCamera>, Without<Player>),
>;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "wakeful".into(),
                resolution: WindowResolution::new(screen::GAME_WIDTH * 2, screen::GAME_HEIGHT * 2),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(RonAssetPlugin::<Scene>::new(&["scene"]))
        .insert_resource(ClearColor(Color::srgb(0.10, 0.08, 0.13)))
        .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
        .add_systems(
            Startup,
            (
                screen::setup_screen,
                setup_game_camera,
                spawn_world,
                spawn_player,
                load_scene,
            )
                .chain(),
        )
        .add_systems(
            Update,
            (
                quit_on_escape,
                screen::resize_present,
                apply_scene,
                apply_player_model,
            ),
        )
        .add_systems(FixedUpdate, move_player)
        .run();
}

fn quit_on_escape(keys: Res<ButtonInput<KeyCode>>, mut exit: MessageWriter<AppExit>) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.write(AppExit::Success);
    }
}

/// FF7-style fixed view. The pose is overwritten by the scene once it loads;
/// runs behind the background camera so the background shows through.
fn setup_game_camera(mut commands: Commands, game_image: Res<screen::GameImage>) {
    commands.spawn((
        GameCamera,
        Camera3d::default(),
        Camera {
            order: 1,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderTarget::Image(game_image.0.clone().into()),
        RenderLayers::layer(0),
        Transform::from_xyz(0.0, 6.0, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn load_scene(mut commands: Commands, assets: Res<AssetServer>) {
    commands.insert_resource(CurrentScene(assets.load("scenes/devroom.scene")));
    commands.insert_resource(SceneApplied(false));
}

/// Applies the scene once its file has loaded: camera pose, background
/// layer, and character model choice.
fn apply_scene(
    mut commands: Commands,
    assets: Res<AssetServer>,
    game_image: Res<screen::GameImage>,
    scenes: Res<Assets<Scene>>,
    current: Option<Res<CurrentScene>>,
    applied: Option<ResMut<SceneApplied>>,
    mut cameras: GameCameraQuery,
) {
    let (Some(current), Some(mut applied)) = (current, applied) else {
        return;
    };
    if applied.0 {
        return;
    }
    let Some(scene) = scenes.get(&current.0) else {
        return;
    };

    let Ok((mut transform, mut projection)) = cameras.single_mut() else {
        return;
    };
    *transform = Transform::from_translation(scene.camera.position.into())
        .looking_at(scene.camera.target.into(), Vec3::Y);
    *projection = Projection::Perspective(PerspectiveProjection {
        fov: scene.camera.fov_degrees.to_radians(),
        aspect_ratio: screen::GAME_WIDTH as f32 / screen::GAME_HEIGHT as f32,
        ..default()
    });

    // The background camera always clears the image (with the global clear
    // color) so the 3D camera can draw over it without clearing.
    commands.spawn((
        Camera2d,
        Camera {
            order: 0,
            ..default()
        },
        RenderTarget::Image(game_image.0.clone().into()),
        RenderLayers::layer(BG_LAYER),
    ));

    if let Some(path) = &scene.background {
        commands.spawn((
            Sprite {
                image: assets.load(path),
                // Backgrounds are authored at the virtual resolution.
                custom_size: Some(Vec2::new(
                    screen::GAME_WIDTH as f32,
                    screen::GAME_HEIGHT as f32,
                )),
                ..default()
            },
            RenderLayers::layer(BG_LAYER),
        ));
    }

    if let Some(path) = &scene.character_model {
        commands.insert_resource(PlayerModel(assets.load(path)));
    }

    applied.0 = true;
}

/// Swaps the placeholder capsule for the scene's character model once the
/// glTF file has loaded.
fn apply_player_model(
    mut commands: Commands,
    model: Option<Res<PlayerModel>>,
    scenes: Res<Assets<WorldAsset>>,
    mut players: Query<Entity, With<Player>>,
) {
    let Some(model) = model else {
        return;
    };
    if scenes.get(&model.0).is_none() {
        return;
    }
    let Ok(player) = players.single_mut() else {
        return;
    };
    commands
        .entity(player)
        .remove::<Mesh3d>()
        .remove::<MeshMaterial3d<StandardMaterial>>()
        .with_child((WorldAssetRoot(model.0.clone()), Transform::default()));
    commands.remove_resource::<PlayerModel>();
}

fn spawn_world(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Placeholder floor; a pre-rendered background image arrives with real art.
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(GROUND_SIZE, GROUND_SIZE))),
        MeshMaterial3d(materials.add(StandardMaterial::from_color(GROUND_COLOR))),
    ));

    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn spawn_player(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn((
        Player,
        Mesh3d(meshes.add(Capsule3d::new(PLAYER_RADIUS, PLAYER_HALF_HEIGHT))),
        MeshMaterial3d(materials.add(StandardMaterial::from_color(PLAYER_COLOR))),
        Transform::from_xyz(0.0, PLAYER_Y, 0.0),
    ));
}

fn move_player(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    scenes: Res<Assets<Scene>>,
    current: Option<Res<CurrentScene>>,
    mut players: Query<&mut Transform, With<Player>>,
) {
    let Ok(mut transform) = players.single_mut() else {
        return;
    };

    // Arrows map onto the ground plane as seen by the fixed camera:
    // up walks away from the camera, down walks toward it.
    let mut direction = Vec2::ZERO;
    if keys.pressed(KeyCode::ArrowUp) {
        direction.y -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowDown) {
        direction.y += 1.0;
    }
    if keys.pressed(KeyCode::ArrowLeft) {
        direction.x -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowRight) {
        direction.x += 1.0;
    }

    let from = transform.translation.xz();
    let moved = move_position(from, direction, PLAYER_SPEED, time.delta_secs());

    // The scene's walkable grid bounds where the player may go; sliding
    // along blocked cells keeps movement feeling responsive.
    let moved = current
        .as_ref()
        .and_then(|c| scenes.get(&c.0))
        .and_then(|scene| scene.walkable.as_ref())
        .map(|grid| grid.constrain(from, moved))
        .unwrap_or(moved);

    transform.translation = Vec3::new(moved.x, PLAYER_Y, moved.y);
}
