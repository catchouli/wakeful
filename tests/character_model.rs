//! Validates the generated character and chest models against the same
//! glTF parser bevy's loader builds on (`bevy_gltf` wraps `gltf::import`).
//!
//! The `tools/generate_*.py` scripts hand-roll the GLB binaries, so
//! format mistakes — mismatched accessor types, out-of-bounds indices,
//! broken keyframe buffers — only surface at parse time. These tests
//! are that parse, plus the rig contracts the animation engine relies
//! on.

use std::collections::HashSet;

/// Parses and structurally validates a generated model: in-bounds
/// indexed meshes, LINEAR one-shot/loop clips targeting named nodes.
fn assert_valid_model(name: &str, bytes: &[u8]) -> gltf::Document {
    // Parses the container, validates every accessor/bufferView against
    // the declared component types, and loads the buffers. Malformed
    // data — the class of bug a hand-rolled writer can ship — fails
    // right here.
    let (document, buffers, _) =
        gltf::import_slice(bytes).unwrap_or_else(|e| panic!("{name} must be a valid glTF: {e}"));

    // Every mesh's indices reference real vertices (the loader panics
    // on violations, so assert it here where the message is readable).
    for mesh in document.meshes() {
        for primitive in mesh.primitives() {
            let reader =
                primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data[..]));
            let vertex_count = reader
                .read_positions()
                .expect("primitives have POSITION")
                .count();
            let indices: Vec<u32> = reader
                .read_indices()
                .expect("primitives are indexed")
                .into_u32()
                .collect();
            assert_eq!(indices.len() % 3, 0, "triangulated indices");
            assert!(
                indices.iter().all(|&i| i < vertex_count as u32),
                "mesh '{}' has out-of-bounds indices",
                mesh.name().unwrap_or("?")
            );
        }
    }

    // Clips must have keyframes targeting actual named rig nodes.
    for animation in document.animations() {
        assert!(
            animation.channels().count() > 0,
            "clip '{}' has no channels",
            animation.name().unwrap_or("?")
        );
        for channel in animation.channels() {
            assert!(
                channel.target().node().name().is_some(),
                "clip '{}' targets an unnamed node",
                animation.name().unwrap_or("?")
            );
            assert!(
                matches!(
                    channel.sampler().interpolation(),
                    gltf::animation::Interpolation::Linear
                ),
                "clips bake LINEAR keyframes"
            );
        }
    }
    document
}

const RIG_JOINTS: [&str; 8] = [
    "hips", "torso", "head", "hair", "arm_l", "arm_r", "leg_l", "leg_r",
];
const CLIPS: [&str; 6] = ["idle", "walk", "run", "pick_up", "shrug", "wave"];

#[test]
fn the_generated_character_model_is_a_valid_rigged_gltf() {
    let document = assert_valid_model(
        "character.glb",
        include_bytes!("../assets/models/character.glb"),
    );

    // The rig: every standard joint present by name.
    let names: HashSet<&str> = document.nodes().filter_map(|n| n.name()).collect();
    for joint in RIG_JOINTS {
        assert!(names.contains(joint), "rig is missing the '{joint}' joint");
    }

    // The full standard clip set: locomotion loops the engine repeats,
    // emotes are one-shots.
    let animation_names: HashSet<&str> = document.animations().filter_map(|a| a.name()).collect();
    for clip in CLIPS {
        assert!(animation_names.contains(clip), "missing the '{clip}' clip");
    }
    assert_eq!(
        document.animations().count(),
        CLIPS.len(),
        "no extra unnamed animations"
    );
}

const CHEST_PARTS: [&str; 3] = ["character", "base", "lid"];

#[test]
fn the_generated_chest_model_opens_with_a_hinged_lid() {
    let document = assert_valid_model("chest.glb", include_bytes!("../assets/models/chest.glb"));

    let names: HashSet<&str> = document.nodes().filter_map(|n| n.name()).collect();
    for part in CHEST_PARTS {
        assert!(names.contains(part), "chest is missing the '{part}' node");
    }

    // Exactly the open clip, targeting the lid.
    let animations: Vec<_> = document.animations().collect();
    assert_eq!(animations.len(), 1, "the chest ships only 'open'");
    assert_eq!(animations[0].name(), Some("open"));
    let targets: HashSet<&str> = animations[0]
        .channels()
        .map(|c| c.target().node().name().unwrap())
        .collect();
    assert!(targets.contains("lid"), "open must animate the lid");
}
