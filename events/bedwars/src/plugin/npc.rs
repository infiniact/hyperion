//! Talkable AI NPC demo (step B of "active animals + NPCs").
//!
//! A villager spawns in the village, walks toward the nearest player, and when
//! a player right-clicks it (the `Interact` action of [`PlayerInteractEntity`])
//! it forwards a prompt to the AI sidecar via [`AiBridge::ask`] — so the NPC
//! "talks" using the same agent that powers `/ai`.

use bevy::prelude::*;
use hyperion::{
    glam::Vec3,
    net::{Channel, Compose, ConnectionId, DataBundle},
    simulation::{Pitch, Position, Uuid, Velocity, Yaw, entity_kind::EntityKind, packet},
    valence_protocol::{
        ByteAngle, VarInt,
        packets::play::{self, player_interact_entity_c2s::EntityInteraction},
    },
};
use hyperion_utils::EntityExt;
use tracing::info;

use crate::ai::AiBridge;

/// Marks the village-guide NPC.
#[derive(Component)]
pub struct Npc;

const NPC_SPEED: f32 = 0.11; // a touch faster than the animals so it keeps up
const FOLLOW_MIN: f32 = 3.0; // stop this close
const FOLLOW_MAX: f32 = 28.0; // only follow players within this range
const TALK_COOLDOWN_TICKS: i64 = 40; // ~2s between AI replies (anti-spam)

fn spawn_npc(mut commands: Commands<'_, '_>, mut done: Local<'_, bool>) {
    if *done {
        return;
    }
    *done = true;
    // Defaults to the village crossroads; override with `HYPERION_NPC_POS="x,y,z"`
    // to place the guide on a different map.
    let pos = crate::plugin::parse_vec3_env("HYPERION_NPC_POS")
        .unwrap_or_else(|| Vec3::new(1.5, 64.0, 5.5));
    commands.spawn((
        Uuid::new_v4(),
        Position::new(pos.x, pos.y, pos.z),
        Yaw::new(0.0),
        Pitch::new(0.0),
        Velocity::new(0.0, 0.0, 0.0),
        EntityKind::Villager,
        Channel,
        Npc,
    ));
    info!("spawned village-guide NPC");
}

/// Walk the NPC toward the nearest player and broadcast the movement.
fn npc_follow(
    compose: Res<'_, Compose>,
    players: Query<'_, '_, &Position, With<ConnectionId>>,
    mut npcs: Query<'_, '_, (Entity, &mut Position, &mut Yaw), (With<Npc>, Without<ConnectionId>)>,
) {
    for (entity, mut pos, mut yaw) in &mut npcs {
        // Nearest player (horizontal distance).
        let mut nearest: Option<(f32, Vec3)> = None;
        for player_pos in &players {
            let d = Vec3::new(player_pos.x - pos.x, 0.0, player_pos.z - pos.z).length();
            if nearest.is_none_or(|(best, _)| d < best) {
                nearest = Some((d, **player_pos));
            }
        }

        if let Some((dist, target)) = nearest {
            let to = Vec3::new(target.x - pos.x, 0.0, target.z - pos.z);
            if to.length() > 0.01 {
                **yaw = to.x.atan2(to.z).to_degrees();
            }
            if (FOLLOW_MIN..FOLLOW_MAX).contains(&dist) {
                let dir = to.normalize();
                **pos = **pos + dir * NPC_SPEED;
            }
        }

        broadcast_move(&compose, entity, *pos, **yaw);
    }
}

/// Right-click the NPC → ask the AI sidecar to answer as the NPC.
fn npc_interactions(
    mut packets: EventReader<'_, '_, packet::play::PlayerInteractEntity>,
    npcs: Query<'_, '_, Entity, With<Npc>>,
    players: Query<'_, '_, (&ConnectionId, &Position)>,
    ai: Res<'_, AiBridge>,
    compose: Res<'_, Compose>,
    mut last_tick: Local<'_, i64>,
) {
    for packet in packets.read() {
        // A right-click sends Interact(hand) (+ InteractAt); handle Interact only.
        if !matches!(packet.interact, EntityInteraction::Interact(_)) {
            continue;
        }
        let target_id = packet.entity_id.0;
        if !npcs.iter().any(|e| e.minecraft_id() == target_id) {
            continue; // not our NPC
        }

        let tick = compose.global().tick;
        if tick - *last_tick < TALK_COOLDOWN_TICKS {
            continue;
        }
        *last_tick = tick;

        let Ok((&connection, pos)) = players.get(packet.sender()) else {
            continue;
        };

        let prompt = "[You are a friendly village-guide NPC in this Minecraft village. A \
            player just walked up and interacted with you. Greet them warmly in ONE short \
            sentence, and offer to build something nearby if they ask.]"
            .to_string();
        let _ = ai.ask(connection, prompt, Some([pos.x, pos.y, pos.z]));
    }
}

/// Broadcast an entity's position + head rotation to subscribed players.
fn broadcast_move(compose: &Compose, entity: Entity, pos: Position, yaw: f32) {
    let entity_id = VarInt(entity.minecraft_id());
    let mut bundle = DataBundle::new(compose);
    if bundle
        .add_packet(&play::EntityPositionS2c {
            entity_id,
            position: pos.as_dvec3(),
            yaw: ByteAngle::from_degrees(yaw),
            pitch: ByteAngle::from_degrees(0.0),
            on_ground: true,
        })
        .and_then(|()| {
            bundle.add_packet(&play::EntitySetHeadYawS2c {
                entity_id,
                head_yaw: ByteAngle::from_degrees(yaw),
            })
        })
        .and_then(|()| bundle.broadcast_channel(entity.into()))
        .is_err()
    {
        // best-effort cosmetic sync
    }
}

pub struct NpcPlugin;

impl Plugin for NpcPlugin {
    fn build(&self, app: &mut App) {
        if crate::plugin::is_skyblock() {
            return;
        }
        app.add_systems(FixedUpdate, (spawn_npc, npc_follow, npc_interactions));
    }
}
