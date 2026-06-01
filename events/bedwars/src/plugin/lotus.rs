//! "步步生莲" demo: a thin lotus of carpet sits on the ground directly beneath
//! the player and follows them around; the ground they leave is restored. The
//! pad is placed on the *ground surface* (found by scanning down), never at the
//! player's feet — so jumping doesn't strand them on a floating pad.
//! Demonstrates Hyperion's runtime block-edit API ([`Blocks::get_block`] /
//! [`Blocks::set_block`]).

use bevy::prelude::*;
use hyperion::{
    ingress,
    simulation::{Position, blocks::Blocks, packet_state},
    valence_protocol::{BlockState, math::IVec3},
};

/// Per-player lotus state.
#[derive(Component, Default)]
pub struct LotusFootprint {
    /// Block position the current pad is centered on (`None` until first bloom).
    center: Option<IVec3>,
    /// Blocks we overwrote, paired with their original state, so we can restore
    /// the ground once the player steps off.
    restore: Vec<(IVec3, BlockState)>,
}

fn init_footprint(
    trigger: Trigger<'_, OnAdd, packet_state::Play>,
    mut commands: Commands<'_, '_>,
) {
    commands
        .entity(trigger.target())
        .insert(LotusFootprint::default());
}

/// Thin carpet colors a lotus can bloom in.
const PETAL_COLORS: [BlockState; 8] = [
    BlockState::PINK_CARPET,
    BlockState::MAGENTA_CARPET,
    BlockState::RED_CARPET,
    BlockState::PURPLE_CARPET,
    BlockState::LIGHT_BLUE_CARPET,
    BlockState::CYAN_CARPET,
    BlockState::LIME_CARPET,
    BlockState::ORANGE_CARPET,
];
const CORE_COLOR: BlockState = BlockState::YELLOW_CARPET;

/// How far below the feet to look for solid ground.
const GROUND_SCAN_DEPTH: i32 = 16;

/// Is this one of the carpets we lay down? Such blocks are skipped when looking
/// for the ground, so the pad never stacks on itself.
fn is_lotus_carpet(state: BlockState) -> bool {
    state == CORE_COLOR || PETAL_COLORS.contains(&state)
}

fn step_lotus(
    mut blocks: ResMut<'_, Blocks>,
    mut query: Query<'_, '_, (&Position, &mut LotusFootprint)>,
) {
    for (position, mut footprint) in &mut query {
        let x = position.x.floor() as i32;
        let z = position.z.floor() as i32;
        let foot_y = (position.y + 0.05).floor() as i32;

        // Find the ground: scan down from the feet for the first solid block
        // that isn't one of our own carpets. The pad goes one block above it.
        let mut ground_y = None;
        for y in (foot_y - GROUND_SCAN_DEPTH..=foot_y).rev() {
            match blocks.get_block(IVec3::new(x, y, z)) {
                Some(b) if b != BlockState::AIR && !is_lotus_carpet(b) => {
                    ground_y = Some(y);
                    break;
                }
                _ => {}
            }
        }
        let Some(ground_y) = ground_y else {
            continue; // over the void / unloaded — leave the pad where it is
        };
        let center = IVec3::new(x, ground_y + 1, z);

        // Pad already on this ground spot — nothing to do (jumping keeps it put).
        if footprint.center == Some(center) {
            continue;
        }

        // 1. Restore the previous pad in full first. Transient: the lotus is a
        //    cosmetic effect and must never end up in the persisted world.
        for (pos, original) in std::mem::take(&mut footprint.restore) {
            let _ = blocks.set_block_transient(pos, original);
        }

        // 2. Bloom a fresh lotus on the ground: a random petal color around a
        //    yellow stamen. Remember what we overwrite so we can restore it.
        footprint.center = Some(center);
        let petal = PETAL_COLORS[fastrand::usize(..PETAL_COLORS.len())];
        for dz in -1..=1 {
            for dx in -1..=1 {
                let state = if dx == 0 && dz == 0 { CORE_COLOR } else { petal };
                let pos = IVec3::new(center.x + dx, center.y, center.z + dz);
                let Some(original) = blocks.get_block(pos) else {
                    continue; // unloaded chunk
                };
                // Only lay carpet over open space (don't bury walls/trees).
                if original != BlockState::AIR {
                    continue;
                }
                if blocks.set_block_transient(pos, state).is_ok() {
                    footprint.restore.push((pos, original));
                }
            }
        }
    }
}

pub struct LotusPlugin;

impl Plugin for LotusPlugin {
    fn build(&self, app: &mut App) {
        if crate::plugin::is_skyblock() {
            return;
        }
        app.add_observer(init_footprint);
        app.add_systems(FixedUpdate, step_lotus.after(ingress::decode::play));
    }
}
