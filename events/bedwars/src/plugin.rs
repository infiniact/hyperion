pub mod attack;
pub mod block;
pub mod bow;
pub mod chat;
pub mod cityweather;
pub mod creatures;
pub mod damage;
pub mod lotus;
pub mod npc;
pub mod regeneration;
pub mod skyblock;
pub mod spawn;
pub mod stats;
pub mod vanish;
pub mod weather;

use hyperion::glam::Vec3;

/// True when running in skyblock mode (`HYPERION_GAMEMODE=skyblock`). The
/// village demo plugins (roaming animals, guide NPC, step-lotus) skip themselves
/// so they don't wander off the tiny island.
pub(crate) fn is_skyblock() -> bool {
    std::env::var("HYPERION_GAMEMODE").as_deref() == Ok("skyblock")
}

/// Parse an `"x,y,z"` env var into a [`Vec3`], if set and well-formed. Lets the
/// demo plugins (NPC, creatures) take their spawn anchor from the environment
/// instead of hardcoding coordinates tied to one specific map.
pub(crate) fn parse_vec3_env(key: &str) -> Option<Vec3> {
    let raw = std::env::var(key).ok()?;
    let mut parts = raw.split(',').map(|s| s.trim().parse::<f32>());
    let x = parts.next()?.ok()?;
    let y = parts.next()?.ok()?;
    let z = parts.next()?.ok()?;
    if parts.next().is_some() {
        return None; // more than 3 components — malformed
    }
    Some(Vec3::new(x, y, z))
}
