//! "步步生莲" demo: when a player walks and then stops, a lotus pad blooms
//! under their feet. Demonstrates Hyperion's runtime block-edit API
//! ([`Blocks::set_block`]) — edits are auto-rebroadcast to nearby players.

use bevy::prelude::*;
use hyperion::{
    ingress,
    simulation::{Position, blocks::Blocks, packet_state},
    valence_protocol::{BlockState, math::IVec3},
};

/// Per-player movement state driving the lotus effect.
#[derive(Component, Default)]
pub struct LotusTracker {
    /// Horizontal block position (x, z) observed last tick.
    last_xz: Option<(i32, i32)>,
    /// True once the player has moved since the last bloom.
    moved: bool,
    /// Center of the last lotus, so standing still doesn't re-bloom forever.
    last_lotus: Option<IVec3>,
}

fn init_tracker(
    trigger: Trigger<'_, OnAdd, packet_state::Play>,
    mut commands: Commands<'_, '_>,
) {
    commands
        .entity(trigger.target())
        .insert(LotusTracker::default());
}

// 7x7 petal mask: 0 = nothing, 1 = outer petal, 2 = inner petal, 3 = core.
const MASK: [[u8; 7]; 7] = [
    [0, 0, 1, 1, 1, 0, 0],
    [0, 1, 1, 2, 1, 1, 0],
    [1, 1, 2, 2, 2, 1, 1],
    [1, 2, 2, 3, 2, 2, 1],
    [1, 1, 2, 2, 2, 1, 1],
    [0, 1, 1, 2, 1, 1, 0],
    [0, 0, 1, 1, 1, 0, 0],
];

const OUTER_PETAL: BlockState = BlockState::PINK_WOOL;
const INNER_PETAL: BlockState = BlockState::MAGENTA_WOOL;
const CORE: BlockState = BlockState::YELLOW_WOOL;

fn bloom_lotus(blocks: &mut Blocks, center: IVec3) {
    for (i, row) in MASK.iter().enumerate() {
        for (j, &kind) in row.iter().enumerate() {
            let state = match kind {
                1 => OUTER_PETAL,
                2 => INNER_PETAL,
                3 => CORE,
                _ => continue,
            };
            let dx = j as i32 - 3;
            let dz = i as i32 - 3;
            let pos = IVec3::new(center.x + dx, center.y, center.z + dz);
            // Ignore failures (e.g. a petal spilling into a not-yet-loaded
            // neighbour chunk): the rest of the flower still blooms.
            let _ = blocks.set_block(pos, state);
        }
    }
}

fn step_lotus(
    mut blocks: ResMut<'_, Blocks>,
    mut query: Query<'_, '_, (&Position, &mut LotusTracker)>,
) {
    for (position, mut tracker) in &mut query {
        let xz = (position.x.floor() as i32, position.z.floor() as i32);

        match tracker.last_xz {
            // Moved to a new block column this tick.
            Some(prev) if prev != xz => {
                tracker.last_xz = Some(xz);
                tracker.moved = true;
            }
            // Same column as last tick: the player has stopped.
            Some(_) => {
                if tracker.moved {
                    tracker.moved = false;
                    // Block the player is standing on (feet floor - 1).
                    let ground_y = position.y.floor() as i32 - 1;
                    let center = IVec3::new(xz.0, ground_y, xz.1);
                    if tracker.last_lotus != Some(center) {
                        tracker.last_lotus = Some(center);
                        bloom_lotus(&mut blocks, center);
                    }
                }
            }
            // First observation.
            None => tracker.last_xz = Some(xz),
        }
    }
}

pub struct LotusPlugin;

impl Plugin for LotusPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(init_tracker);
        app.add_systems(FixedUpdate, step_lotus.after(ingress::decode::play));
    }
}
