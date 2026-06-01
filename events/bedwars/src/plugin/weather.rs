//! Extreme-weather system: a server-driven storm/earthquake that shows on every
//! player's screen (action-bar HUD), strikes lightning, blows players around,
//! and **damages buildings** via the runtime block API.
//!
//! Damage can be transient (restored on restart) or permanent, toggled at
//! runtime. Triggered by the `/weather` command (see `command/weather.rs`) and,
//! later, by real-world weather.

use std::time::Duration;

use bevy::prelude::*;
use hyperion::{
    glam::{I16Vec2, IVec3, Vec3},
    net::{Channel, Compose},
    runtime::AsyncRuntime,
    simulation::{
        Pitch, Position, Uuid, Velocity, Yaw, blocks::Blocks, entity_kind::EntityKind,
    },
    valence_protocol::{
        BlockPos, BlockState,
        block::BlockKind,
        packets::play,
        text::IntoText,
    },
};
use tracing::{info, warn};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WeatherKind {
    Calm,
    Storm,
    Quake,
}

/// Global weather state. Mutated by the `/weather` command and the (later)
/// real-world poller; read by [`weather_tick`].
#[derive(Resource)]
pub struct Weather {
    pub kind: WeatherKind,
    /// 0..=12 (Beaufort-ish). Drives effect rates and HUD.
    pub intensity: u8,
    /// Remaining ticks of the current event (0 when calm).
    pub ticks_left: u32,
    /// `true` = block damage is saved (permanent ruins); `false` = transient
    /// (world heals on restart).
    pub persist_damage: bool,
    /// `true` = follow real-world weather (the poller drives storms).
    pub auto_follow: bool,
    /// Countdown to the next effect pulse (lightning/wind/damage).
    next_pulse: u32,
}

impl Default for Weather {
    fn default() -> Self {
        Self {
            kind: WeatherKind::Calm,
            intensity: 0,
            ticks_left: 0,
            persist_damage: false,
            auto_follow: true,
            next_pulse: 0,
        }
    }
}

impl Weather {
    /// Start an event of `kind` at `intensity` (1..=12) for `seconds`.
    pub fn start(&mut self, kind: WeatherKind, intensity: u8, seconds: u32) {
        self.kind = kind;
        self.intensity = intensity.clamp(1, 12);
        self.ticks_left = seconds * 20;
        self.next_pulse = 0;
    }

    pub fn calm(&mut self) {
        self.kind = WeatherKind::Calm;
        self.intensity = 0;
        self.ticks_left = 0;
    }
}

/// Lightning bolts are short-lived; despawn after the bolt animation plays.
#[derive(Component)]
struct Bolt(u32);

const LIGHTNING_TTL: u32 = 10;

#[expect(clippy::too_many_arguments)]
fn weather_tick(
    mut weather: ResMut<'_, Weather>,
    compose: Res<'_, Compose>,
    mut blocks: ResMut<'_, Blocks>,
    mut players: Query<'_, '_, (&Position, &mut Velocity)>,
    mut commands: Commands<'_, '_>,
) {
    if weather.kind == WeatherKind::Calm || weather.ticks_left == 0 {
        if weather.kind != WeatherKind::Calm && weather.ticks_left == 0 {
            // Event just ended.
            weather.calm();
            let pkt = play::GameMessageS2c {
                chat: "§aThe weather has calmed.".into_cow_text(),
                overlay: true,
            };
            let _ok = compose.broadcast(&pkt).send();
        }
        return;
    }
    weather.ticks_left -= 1;

    // --- on-screen HUD (action bar), refreshed ~2x/sec so it never fades ---
    if weather.ticks_left % 10 == 0 {
        let filled = usize::from(weather.intensity);
        let bar: String = "▰".repeat(filled) + &"▱".repeat(12 - filled);
        let (icon, name, color) = match weather.kind {
            WeatherKind::Storm => ("⚡", "Storm", "§c"),
            WeatherKind::Quake => ("§6☶§r", "Quake", "§6"),
            WeatherKind::Calm => ("", "", "§a"),
        };
        let secs = weather.ticks_left / 20;
        let text = format!("{color}{icon} {name} Lv{} §8[{bar}{color}§8] §7{secs}s", weather.intensity);
        let pkt = play::GameMessageS2c {
            chat: text.into_cow_text(),
            overlay: true,
        };
        let _ok = compose.broadcast(&pkt).send();
    }

    // --- despawn finished lightning is handled by `cleanup_bolts` ---

    // --- effect pulses, faster at higher intensity ---
    if weather.next_pulse > 0 {
        weather.next_pulse -= 1;
        return;
    }
    // higher intensity → shorter gap between pulses (12 → every 5 ticks)
    weather.next_pulse = (40u32).saturating_sub(u32::from(weather.intensity) * 3).max(5);

    let intensity = weather.intensity;
    let kind = weather.kind;
    let persist = weather.persist_damage;

    // Player positions are the anchors that effects happen around.
    let anchors: Vec<Vec3> = players.iter().map(|(p, _)| **p).collect();
    if anchors.is_empty() {
        return;
    }

    // --- lightning: occasional single strike near a random player (storm) ---
    if kind == WeatherKind::Storm && fastrand::u8(0..60) < intensity {
        let anchor = anchors[fastrand::usize(..anchors.len())];
        let lx = anchor.x + (fastrand::f32() - 0.5) * 24.0;
        let lz = anchor.z + (fastrand::f32() - 0.5) * 24.0;
        commands.spawn((
            Uuid::new_v4(),
            Position::new(lx, anchor.y, lz),
            Yaw::new(0.0),
            Pitch::new(0.0),
            Velocity::new(0.0, 0.0, 0.0),
            EntityKind::Lightning,
            Channel,
            Bolt(LIGHTNING_TTL),
        ));
    }

    // --- building damage: chip away solid blocks around each player ---
    let hits = 1 + u32::from(intensity) / 2;
    for &anchor in &anchors {
        for _ in 0..hits {
            let radius = 18.0;
            let bx = (anchor.x + (fastrand::f32() - 0.5) * 2.0 * radius).floor() as i32;
            let bz = (anchor.z + (fastrand::f32() - 0.5) * 2.0 * radius).floor() as i32;
            // quake gnaws low (foundations), storm chews from the top down
            let (y_hi, y_lo) = if kind == WeatherKind::Quake { (66, 60) } else { (90, 65) };
            for by in (y_lo..=y_hi).rev() {
                let pos = IVec3::new(bx, by, bz);
                let Some(state) = blocks.get_block(pos) else {
                    continue;
                };
                if !is_structural(state) {
                    continue;
                }
                let ok = if persist {
                    blocks.set_block(pos, BlockState::AIR).is_ok()
                } else {
                    blocks.set_block_transient(pos, BlockState::AIR).is_ok()
                };
                if ok {
                    broadcast_block(&compose, pos, BlockState::AIR);
                }
                break; // one block per ray
            }
        }
    }

    // --- wind / quake: shove every player by adding to their Velocity (the
    //     player-sync egress then sends it to the client, like attack knockback) ---
    let strength = f32::from(intensity);
    for (_pos, mut velocity) in &mut players {
        let impulse = match kind {
            WeatherKind::Storm => {
                let a = fastrand::f32() * std::f32::consts::TAU;
                Vec3::new(a.cos() * strength * 0.02, 0.12, a.sin() * strength * 0.02)
            }
            // quake: vertical jolt + small horizontal jitter
            WeatherKind::Quake => Vec3::new(
                (fastrand::f32() - 0.5) * strength * 0.015,
                strength * 0.025,
                (fastrand::f32() - 0.5) * strength * 0.015,
            ),
            WeatherKind::Calm => Vec3::ZERO,
        };
        velocity.0 += impulse;
    }
}

/// Blocks the weather is allowed to destroy: built materials and trees, never
/// the terrain itself (grass/dirt/bedrock/water).
fn is_structural(state: BlockState) -> bool {
    let kind = state.to_kind();
    !matches!(
        kind,
        BlockKind::Air
            | BlockKind::CaveAir
            | BlockKind::VoidAir
            | BlockKind::Bedrock
            | BlockKind::GrassBlock
            | BlockKind::Dirt
            | BlockKind::Water
            | BlockKind::Lava
            | BlockKind::Sand
            | BlockKind::Gravel
            | BlockKind::Stone
    )
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

/// Tick down and despawn spent lightning bolts.
fn cleanup_bolts(mut commands: Commands<'_, '_>, mut bolts: Query<'_, '_, (Entity, &mut Bolt)>) {
    for (entity, mut bolt) in &mut bolts {
        if bolt.0 == 0 {
            commands.entity(entity).despawn();
        } else {
            bolt.0 -= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 2: real-world weather → in-game storm
// ---------------------------------------------------------------------------

/// A condition derived from real weather, sent from the poll task to the game.
#[derive(Clone, Copy)]
enum RealCond {
    Storm(u8),
    Calm,
}

/// Receiver for real-weather updates (drained on the game thread).
#[derive(Resource)]
struct RealWeatherRx(flume::Receiver<RealCond>);

/// Map km/h wind speed to a Beaufort number (0..=12).
fn beaufort(kmh: f64) -> u8 {
    const T: [f64; 12] = [1.0, 6.0, 12.0, 20.0, 29.0, 39.0, 50.0, 62.0, 75.0, 89.0, 103.0, 118.0];
    T.iter().filter(|&&t| kmh >= t).count() as u8
}

fn classify(weather_code: i64, wind_kmh: f64) -> RealCond {
    let level = beaufort(wind_kmh);
    // WMO thunderstorm codes
    let thunder = matches!(weather_code, 95 | 96 | 99);
    if thunder {
        RealCond::Storm(level.max(8))
    } else if level >= 6 {
        // strong breeze and up → a visible storm
        RealCond::Storm(level)
    } else {
        RealCond::Calm
    }
}

/// Spawn the background poll task once. Opt out with `HYPERION_WEATHER=off`.
fn start_real_weather_poll(
    runtime: Res<'_, AsyncRuntime>,
    mut commands: Commands<'_, '_>,
    mut done: Local<'_, bool>,
) {
    if *done {
        return;
    }
    *done = true;

    if std::env::var("HYPERION_WEATHER").as_deref() == Ok("off") {
        info!("real-world weather following disabled (HYPERION_WEATHER=off)");
        return;
    }
    let lat = env_f64("HYPERION_WEATHER_LAT", 39.9);
    let lon = env_f64("HYPERION_WEATHER_LON", 116.4);

    let (tx, rx) = flume::unbounded::<RealCond>();
    commands.insert_resource(RealWeatherRx(rx));

    runtime.spawn(async move {
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
             &current=weather_code,wind_speed_10m&wind_speed_unit=kmh"
        );
        loop {
            match hyperion_utils::fetch_text(&url).await {
                Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(json) => {
                        let cur = &json["current"];
                        let code = cur["weather_code"].as_i64().unwrap_or(0);
                        let wind = cur["wind_speed_10m"].as_f64().unwrap_or(0.0);
                        let cond = classify(code, wind);
                        info!("real weather: code={code} wind={wind}km/h → beaufort {}", beaufort(wind));
                        let _ok = tx.send_async(cond).await;
                    }
                    Err(e) => warn!("weather parse failed: {e}"),
                },
                Err(e) => warn!("weather fetch failed: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(300)).await; // every 5 min
        }
    });
}

/// Drain real-weather updates and (if auto-follow is on) apply them.
fn apply_real_weather(rx: Option<Res<'_, RealWeatherRx>>, mut weather: ResMut<'_, Weather>) {
    let Some(rx) = rx else {
        return;
    };
    for cond in rx.0.try_iter() {
        if !weather.auto_follow {
            continue;
        }
        match cond {
            // Run a bit longer than the poll interval so it persists between polls.
            RealCond::Storm(level) => weather.start(WeatherKind::Storm, level.max(1), 360),
            RealCond::Calm => weather.calm(),
        }
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub struct WeatherPlugin;

impl Plugin for WeatherPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Weather::default());
        app.add_systems(
            FixedUpdate,
            (
                start_real_weather_poll,
                apply_real_weather,
                weather_tick,
                cleanup_bolts,
            ),
        );
    }
}
