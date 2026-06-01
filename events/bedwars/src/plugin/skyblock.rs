//! Skyblock gameplay (shared-island MVP).
//!
//! Works with the void world from `tools/mapgen/gen_skyblock_map.py`:
//!  - fall into the void → teleported back to the island
//!  - join → a small starter kit
//!  - a cobblestone "generator" block regenerates a short while after it's mined
//!    (Hyperion has no fluid simulation, so the classic lava+water generator
//!    can't form; this gives the same infinite-cobble loop instead).

use bevy::prelude::*;
use hyperion::{
    ItemKind, ItemStack,
    glam::{I16Vec2, IVec3, Vec3},
    net::{Compose, ConnectionId, agnostic},
    simulation::{PendingTeleportation, Position, blocks::Blocks, event, packet},
    valence_protocol::{BlockPos, BlockState, block::BlockKind, packets::play},
};
use hyperion_inventory::PlayerInventory;
use tracing::info;

const VOID_Y: f32 = -10.0;
const ISLAND_SPAWN: Vec3 = Vec3::new(8.5, 64.0, 8.5);
/// The cobblestone generator block on the starter island.
const GEN_POS: IVec3 = IVec3::new(12, 64, 4);
/// The starter chest block — right-click once for a bonus kit.
const CHEST_POS: IVec3 = IVec3::new(8, 64, 8);
/// Ticks before a mined generator block regrows (~1.5s).
const REGEN_TICKS: i64 = 30;

/// Pending cobblestone regenerations: (position, tick when it regrows).
#[derive(Resource, Default)]
struct Regens(Vec<(IVec3, i64)>);

/// Teleport players who fall into the void back onto the island.
fn void_respawn(
    compose: Res<'_, Compose>,
    mut commands: Commands<'_, '_>,
    mut query: Query<'_, '_, (Entity, &mut Position, &ConnectionId)>,
) {
    for (entity, mut pos, &connection) in &mut query {
        if pos.y < VOID_Y {
            **pos = ISLAND_SPAWN;
            commands
                .entity(entity)
                .insert(PendingTeleportation::new(ISLAND_SPAWN));
            let _ok = compose.unicast(&agnostic::chat("§c你掉进了虚空,已送回小岛"), connection);
        }
    }
}

/// Give a starter kit the moment a player's inventory is created.
fn give_starter_kit(
    trigger: Trigger<'_, OnAdd, PlayerInventory>,
    mut query: Query<'_, '_, &mut PlayerInventory>,
) {
    let Ok(mut inv) = query.get_mut(trigger.target()) else {
        return;
    };
    for (kind, count) in [
        (ItemKind::WoodenPickaxe, 1),
        (ItemKind::WoodenAxe, 1),
        (ItemKind::OakSapling, 4),
        (ItemKind::Dirt, 16),
        (ItemKind::Bread, 8),
    ] {
        inv.try_add_item(ItemStack::new(kind, count, None));
    }
    info!("gave skyblock starter kit");
}

/// Marks players who have already claimed the starter chest (claim-once).
#[derive(Component)]
struct StarterChestClaimed;

/// Right-click the island's starter chest → a one-time bonus kit.
fn open_starter_chest(
    mut packets: EventReader<'_, '_, packet::play::PlayerInteractBlock>,
    mut players: Query<'_, '_, (&mut PlayerInventory, &ConnectionId), Without<StarterChestClaimed>>,
    compose: Res<'_, Compose>,
    mut commands: Commands<'_, '_>,
) {
    for pkt in packets.read() {
        let p = pkt.position;
        if p.x != CHEST_POS.x || p.y != CHEST_POS.y || p.z != CHEST_POS.z {
            continue;
        }
        // `Without<StarterChestClaimed>` → already-claimed players just no-op.
        let Ok((mut inv, &connection)) = players.get_mut(pkt.sender()) else {
            continue;
        };
        for (kind, count) in [
            (ItemKind::OakLog, 16),
            (ItemKind::Bread, 16),
            (ItemKind::IronIngot, 3),
            (ItemKind::OakSapling, 4),
        ] {
            inv.try_add_item(ItemStack::new(kind, count, None));
        }
        commands.entity(pkt.sender()).insert(StarterChestClaimed);
        let _ok = compose.unicast(&agnostic::chat("§a你打开了开局宝箱,获得额外物资!"), connection);
    }
}

/// Schedule a regen when the generator block is mined.
fn on_block_destroyed(
    mut events: EventReader<'_, '_, event::DestroyBlock>,
    compose: Res<'_, Compose>,
    mut regens: ResMut<'_, Regens>,
) {
    let tick = compose.global().tick;
    for ev in events.read() {
        if ev.position == GEN_POS {
            regens.0.push((GEN_POS, tick + REGEN_TICKS));
        }
    }
}

/// Regrow generator blocks whose timer is up.
fn run_regens(
    compose: Res<'_, Compose>,
    mut blocks: ResMut<'_, Blocks>,
    mut regens: ResMut<'_, Regens>,
) {
    let tick = compose.global().tick;
    let cobble = BlockState::from_kind(BlockKind::Cobblestone);
    regens.0.retain(|&(pos, due)| {
        if tick < due {
            return true;
        }
        if blocks.set_block(pos, cobble).is_ok() {
            broadcast_block(&compose, pos, cobble);
        }
        false
    });
}

fn broadcast_block(compose: &Compose, pos: IVec3, state: BlockState) {
    let (Ok(cx), Ok(cz)) = (i16::try_from(pos.x >> 4), i16::try_from(pos.z >> 4)) else {
        return;
    };
    let pkt = play::BlockUpdateS2c {
        position: BlockPos::new(pos.x, pos.y, pos.z),
        block_id: state,
    };
    let _ok = compose.broadcast_local(&pkt, I16Vec2::new(cx, cz)).send();
}

pub struct SkyblockPlugin;

impl Plugin for SkyblockPlugin {
    fn build(&self, app: &mut App) {
        if !crate::plugin::is_skyblock() {
            return; // only active in skyblock mode
        }
        app.insert_resource(Regens::default());
        app.add_observer(give_starter_kit);
        app.add_systems(
            FixedUpdate,
            (void_respawn, on_block_destroyed, run_regens, open_starter_chest),
        );
        info!("skyblock mode enabled");
    }
}
