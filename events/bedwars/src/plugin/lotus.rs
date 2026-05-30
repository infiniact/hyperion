//! "步步生莲" demo: a thin lotus of carpet follows the player's feet — the
//! block they stand on plus the 8 surrounding blocks. As they move, the carpet
//! they left behind is removed and the ground restored. Demonstrates Hyperion's
//! runtime block-edit API ([`Blocks::get_block`] / [`Blocks::set_block`]).

use bevy::prelude::*;
use hyperion::{
    ingress,
    simulation::{Position, blocks::Blocks, packet_state},
    valence_protocol::{BlockState, math::IVec3},
};

/// Per-player lotus state.
#[derive(Component, Default)]
pub struct LotusFootprint {
    /// Block (feet level) the current lotus is centered on (`None` until first
    /// bloom).
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

fn step_lotus(
    mut blocks: ResMut<'_, Blocks>,
    mut query: Query<'_, '_, (&Position, &mut LotusFootprint)>,
) {
    for (position, mut footprint) in &mut query {
        // The block at the player's feet — carpet lays a thin layer on the
        // surface here (+0.05 guards float jitter at integer Y).
        let center = IVec3::new(
            position.x.floor() as i32,
            (position.y + 0.05).floor() as i32,
            position.z.floor() as i32,
        );

        // Lotus already centered here — nothing to do (no churn while standing).
        if footprint.center == Some(center) {
            continue;
        }

        // 1. Restore the ground under the previous lotus, in full, first.
        for (pos, original) in std::mem::take(&mut footprint.restore) {
            let _ = blocks.set_block(pos, original);
        }

        // 2. Bloom a fresh lotus: a random petal color for the 8 surrounding
        //    blocks, a yellow stamen under the player. Remember what we overwrite.
        footprint.center = Some(center);
        let petal = PETAL_COLORS[fastrand::usize(..PETAL_COLORS.len())];
        for dz in -1..=1 {
            for dx in -1..=1 {
                let state = if dx == 0 && dz == 0 {
                    BlockState::YELLOW_CARPET // stamen, under the player
                } else {
                    petal
                };
                let pos = IVec3::new(center.x + dx, center.y, center.z + dz);
                // Capture the original before overwriting; skip not-yet-loaded
                // chunks.
                let Some(original) = blocks.get_block(pos) else {
                    continue;
                };
                if blocks.set_block(pos, state).is_ok() {
                    footprint.restore.push((pos, original));
                }
            }
        }
    }
}

pub struct LotusPlugin;

impl Plugin for LotusPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_footprint);
        app.add_systems(FixedUpdate, step_lotus.after(ingress::decode::play));
    }
}
