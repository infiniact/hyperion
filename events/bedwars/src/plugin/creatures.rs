//! Wandering animals demo (step A of "active animals + NPCs").
//!
//! Hyperion has no vanilla mob AI, so we drive the creatures ourselves: spawn a
//! few vanilla entities ([`EntityKind`]) with a [`Channel`] (so nearby players
//! receive them), then each tick nudge their [`Position`] and broadcast the
//! movement as [`EntityPositionS2c`] — the same way arrows are synced. Entity
//! movement is NOT auto-synced (that path is player-only), hence the manual
//! broadcast here.

use bevy::prelude::*;
use hyperion::{
    glam::Vec3,
    net::{Channel, Compose, DataBundle},
    simulation::{Pitch, Position, Uuid, Velocity, Yaw, entity_kind::EntityKind},
    valence_protocol::{ByteAngle, VarInt, packets::play},
};
use hyperion_utils::EntityExt;
use tracing::info;

/// Per-animal wander state.
#[derive(Component)]
pub struct Wander {
    home: Vec3,
    /// Current horizontal heading (zero = grazing/paused).
    dir: Vec3,
    /// Ticks until the next heading change.
    ticks_left: u32,
}

const SPEED: f32 = 0.08; // blocks per tick (~1.6 blocks/s)
const ROAM_RADIUS: f32 = 18.0;

fn spawn_animals(mut commands: Commands<'_, '_>, mut done: Local<'_, bool>) {
    if *done {
        return;
    }
    *done = true;

    let kinds = [
        EntityKind::Cow,
        EntityKind::Pig,
        EntityKind::Sheep,
        EntityKind::Chicken,
    ];
    // Scattered around the village (ground is grass at Y63 → stand at Y64).
    let spots = [
        (8, 10),
        (-12, 9),
        (14, -8),
        (-10, -12),
        (20, 2),
        (-22, 4),
        (4, -20),
        (-4, 18),
    ];
    // The spots above are offsets around an anchor that defaults to the village
    // area near the origin (ground at Y64). Override with
    // `HYPERION_ANIMALS_CENTER="x,y,z"` to scatter animals on a different map.
    let center = crate::plugin::parse_vec3_env("HYPERION_ANIMALS_CENTER")
        .unwrap_or_else(|| Vec3::new(0.0, 64.0, 0.0));
    for (i, (x, z)) in spots.into_iter().enumerate() {
        let kind = kinds[i % kinds.len()];
        let home = Vec3::new(center.x + x as f32 + 0.5, center.y, center.z + z as f32 + 0.5);
        commands.spawn((
            Uuid::new_v4(),
            Position::new(home.x, home.y, home.z),
            Yaw::new(0.0),
            Pitch::new(0.0),
            Velocity::new(0.0, 0.0, 0.0),
            kind,
            Channel,
            Wander {
                home,
                dir: Vec3::ZERO,
                ticks_left: 0,
            },
        ));
    }
    info!("spawned {} wandering animals", spots.len());
}

fn wander(
    compose: Res<'_, Compose>,
    mut query: Query<'_, '_, (Entity, &mut Position, &mut Yaw, &mut Wander)>,
) {
    for (entity, mut pos, mut yaw, mut wander) in &mut query {
        if wander.ticks_left == 0 {
            // Pick a new heading, or pause to "graze".
            if fastrand::f32() < 0.3 {
                wander.dir = Vec3::ZERO;
            } else {
                let angle = fastrand::f32() * std::f32::consts::TAU;
                wander.dir = Vec3::new(angle.cos(), 0.0, angle.sin());
            }
            wander.ticks_left = 20 + fastrand::u32(0..60);
        }
        wander.ticks_left -= 1;

        if wander.dir != Vec3::ZERO {
            let mut next = **pos + wander.dir * SPEED;
            // Turn back if we'd wander too far from home.
            let off = Vec3::new(next.x - wander.home.x, 0.0, next.z - wander.home.z);
            if off.length() > ROAM_RADIUS {
                wander.dir = -wander.dir;
                next = **pos + wander.dir * SPEED;
            }
            **pos = next;
            **yaw = wander.dir.x.atan2(wander.dir.z).to_degrees();
        }

        // Broadcast the movement to players subscribed to this entity's channel.
        let entity_id = VarInt(entity.minecraft_id());
        let mut bundle = DataBundle::new(&compose);
        if bundle
            .add_packet(&play::EntityPositionS2c {
                entity_id,
                position: pos.as_dvec3(),
                yaw: ByteAngle::from_degrees(**yaw),
                pitch: ByteAngle::from_degrees(0.0),
                on_ground: true,
            })
            .and_then(|()| {
                bundle.add_packet(&play::EntitySetHeadYawS2c {
                    entity_id,
                    head_yaw: ByteAngle::from_degrees(**yaw),
                })
            })
            .and_then(|()| bundle.broadcast_channel(entity.into()))
            .is_err()
        {
            // best-effort cosmetic sync; ignore transient failures
        }
    }
}

pub struct CreaturesPlugin;

impl Plugin for CreaturesPlugin {
    fn build(&self, app: &mut App) {
        if crate::plugin::is_skyblock() {
            return;
        }
        app.add_systems(FixedUpdate, (spawn_animals, wander).chain());
    }
}
