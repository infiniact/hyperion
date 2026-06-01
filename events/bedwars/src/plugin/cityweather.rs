//! Per-player real-world weather widget.
//!
//! Each player sees their own city's weather: an action-bar readout, real
//! client **rain/thunder** (per-client `GameStateChangeS2c`), and drifting
//! **wind particles** blown in the city's real wind direction. Set a city with
//! `/setcity <name>`; until then the server default is shown. Purely cosmetic —
//! the destructive [`super::weather`] event stays global.

use std::borrow::Cow;

use bevy::prelude::*;
use hyperion::{
    glam::{DVec3, Vec3},
    net::{Compose, ConnectionId},
    runtime::AsyncRuntime,
    simulation::{Position, packet_state},
    valence_protocol::{
        packets::play::{
            self,
            game_state_change_s2c::{GameEventKind, GameStateChangeS2c},
            particle_s2c::{Particle, ParticleS2c},
        },
        text::IntoText,
    },
};
use tracing::{info, warn};

use crate::plugin::weather::{Weather, WeatherKind};

/// Precipitation state derived from the weather code — drives real client rain.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Precip {
    Clear,
    Rain,
    Thunder,
}

fn precip_of(code: i64) -> Precip {
    match code {
        95 | 96 | 99 => Precip::Thunder,
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 | 71 | 73 | 75 | 77 | 80 | 81 | 82
        | 85 | 86 => Precip::Rain,
        _ => Precip::Clear,
    }
}

/// One fetched condition, sent from the async poll task to the game thread.
#[derive(Clone)]
struct CityCond {
    text: String,
    precip: Precip,
    wind_level: u8,
    /// Direction the wind blows *from*, in degrees (meteorological).
    wind_dir: f64,
}

/// Per-player weather location + last-fetched condition.
#[derive(Component)]
pub struct CityWeather {
    label: String,
    lat: f64,
    lon: f64,
    text: String,
    precip: Precip,
    applied_precip: Option<Precip>,
    wind_level: u8,
    wind_dir: f64,
    /// Game tick when this player should be polled next (0 = poll now).
    next_poll: i64,
}

impl Default for CityWeather {
    fn default() -> Self {
        Self {
            label: std::env::var("HYPERION_WEATHER_CITY").unwrap_or_else(|_| "北京".to_string()),
            lat: env_f64("HYPERION_WEATHER_LAT", 39.9),
            lon: env_f64("HYPERION_WEATHER_LON", 116.4),
            text: "§7天气加载中…".to_string(),
            precip: Precip::Clear,
            applied_precip: None,
            wind_level: 0,
            wind_dir: 0.0,
            next_poll: 0,
        }
    }
}

pub(crate) type GeoMsg = (Entity, f64, f64, String);

/// Channels bridging async fetch tasks back to the game thread.
#[derive(Resource)]
pub struct CityNet {
    /// `/setcity` geocode results land here.
    pub(crate) geo_tx: flume::Sender<GeoMsg>,
    geo_rx: flume::Receiver<GeoMsg>,
    weather_tx: flume::Sender<(Entity, CityCond)>,
    weather_rx: flume::Receiver<(Entity, CityCond)>,
}

fn init_player(trigger: Trigger<'_, OnAdd, packet_state::Play>, mut commands: Commands<'_, '_>) {
    commands
        .entity(trigger.target())
        .insert(CityWeather::default());
}

const POLL_INTERVAL_TICKS: i64 = 6000; // 5 min @ 20 TPS

/// Spawn per-player weather fetches when due.
fn poll_player_weather(
    compose: Res<'_, Compose>,
    runtime: Res<'_, AsyncRuntime>,
    net: Res<'_, CityNet>,
    mut query: Query<'_, '_, (Entity, &mut CityWeather)>,
) {
    let tick = compose.global().tick;
    for (entity, mut cw) in &mut query {
        if tick < cw.next_poll {
            continue;
        }
        cw.next_poll = tick + POLL_INTERVAL_TICKS;
        let (lat, lon, label) = (cw.lat, cw.lon, cw.label.clone());
        let tx = net.weather_tx.clone();
        runtime.spawn(async move {
            let cond = fetch_city(lat, lon, &label).await;
            let _ok = tx.send_async((entity, cond)).await;
        });
    }
}

/// Apply fetched weather + geocode results; push rain/thunder on change.
fn drain_city_net(
    net: Res<'_, CityNet>,
    compose: Res<'_, Compose>,
    mut query: Query<'_, '_, (&mut CityWeather, &ConnectionId)>,
) {
    for (entity, cond) in net.weather_rx.try_iter() {
        if let Ok((mut cw, &connection)) = query.get_mut(entity) {
            cw.text = cond.text;
            cw.precip = cond.precip;
            cw.wind_level = cond.wind_level;
            cw.wind_dir = cond.wind_dir;
            if cw.applied_precip != Some(cond.precip) {
                cw.applied_precip = Some(cond.precip);
                send_precip(&compose, connection, cond.precip);
            }
        }
    }
    for (entity, lat, lon, label) in net.geo_rx.try_iter() {
        if let Ok((mut cw, _)) = query.get_mut(entity) {
            cw.lat = lat;
            cw.lon = lon;
            cw.label = label;
            cw.next_poll = 0; // refresh immediately
        }
    }
}

/// Push the vanilla rain/thunder game-state to one client (per-player weather).
fn send_precip(compose: &Compose, connection: ConnectionId, precip: Precip) {
    let send = |kind, value: f32| {
        let _ok = compose.unicast(&GameStateChangeS2c { kind, value }, connection);
    };
    match precip {
        Precip::Clear => {
            send(GameEventKind::EndRaining, 0.0);
            send(GameEventKind::ThunderLevelChange, 0.0);
        }
        Precip::Rain => {
            send(GameEventKind::BeginRaining, 0.0);
            send(GameEventKind::RainLevelChange, 0.8);
            send(GameEventKind::ThunderLevelChange, 0.0);
        }
        Precip::Thunder => {
            send(GameEventKind::BeginRaining, 0.0);
            send(GameEventKind::RainLevelChange, 1.0);
            send(GameEventKind::ThunderLevelChange, 0.85);
        }
    }
}

/// Show each player's city weather on the action bar — but only when there's no
/// global storm event (that HUD takes precedence).
fn city_hud(
    compose: Res<'_, Compose>,
    weather: Res<'_, Weather>,
    query: Query<'_, '_, (&CityWeather, &ConnectionId)>,
) {
    if weather.kind != WeatherKind::Calm {
        return;
    }
    if compose.global().tick % 10 != 0 {
        return;
    }
    for (cw, &connection) in &query {
        let pkt = play::GameMessageS2c {
            chat: cw.text.clone().into_cow_text(),
            overlay: true,
        };
        let _ok = compose.unicast(&pkt, connection);
    }
}

/// Blow dust particles past each player in their city's real wind direction,
/// scaled by wind strength. Unicast, so each player sees their own city's wind.
fn wind_particles(
    compose: Res<'_, Compose>,
    query: Query<'_, '_, (&CityWeather, &Position, &ConnectionId)>,
) {
    if compose.global().tick % 3 != 0 {
        return;
    }
    for (cw, pos, &connection) in &query {
        if cw.wind_level < 3 {
            continue; // only show a visible breeze and up
        }
        // Travel direction (MC axes) from the meteorological "from" bearing.
        let from = cw.wind_dir.to_radians();
        let dir = Vec3::new(-(from.sin() as f32), 0.02, from.cos() as f32);
        let speed = 0.4 + f32::from(cw.wind_level) * 0.12;
        let base = pos.as_dvec3();
        for _ in 0..cw.wind_level {
            let position = DVec3::new(
                base.x + (fastrand::f64() - 0.5) * 14.0,
                base.y + 1.0 + fastrand::f64() * 3.0,
                base.z + (fastrand::f64() - 0.5) * 14.0,
            );
            // count = 0 → the client treats `offset` as a velocity (× max_speed).
            // Ash = small dusty specks (Cloud looked like puffy snowflakes).
            let pkt = ParticleS2c {
                particle: Cow::Owned(Particle::Ash),
                long_distance: true,
                position,
                offset: dir,
                max_speed: speed,
                count: 0,
            };
            let _ok = compose.unicast(&pkt, connection);
        }
    }
}

/// Fetch one city's current weather → display text + precip + wind.
async fn fetch_city(lat: f64, lon: f64, label: &str) -> CityCond {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
         &current=weather_code,wind_speed_10m,wind_direction_10m,temperature_2m&wind_speed_unit=kmh"
    );
    let fallback = |msg: &str| CityCond {
        text: format!("§7{label} §c{msg}"),
        precip: Precip::Clear,
        wind_level: 0,
        wind_dir: 0.0,
    };
    match hyperion_utils::fetch_text(&url).await {
        Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(json) => {
                let cur = &json["current"];
                let code = cur["weather_code"].as_i64().unwrap_or(-1);
                let wind = cur["wind_speed_10m"].as_f64().unwrap_or(0.0);
                let wind_dir = cur["wind_direction_10m"].as_f64().unwrap_or(0.0);
                let temp = cur["temperature_2m"].as_f64().unwrap_or(0.0);
                let level = beaufort(wind);
                let (color, desc) = describe(code);
                CityCond {
                    text: format!(
                        "{color}{desc} §f{label} §7{temp:.0}°C · 风{level}级 {wind:.0}km/h"
                    ),
                    precip: precip_of(code),
                    wind_level: level,
                    wind_dir,
                }
            }
            Err(e) => {
                warn!("city weather parse failed: {e}");
                fallback("天气解析失败")
            }
        },
        Err(e) => {
            warn!("city weather fetch failed: {e}");
            fallback("天气获取失败")
        }
    }
}

/// WMO weather code → (color code, Chinese description).
fn describe(code: i64) -> (&'static str, &'static str) {
    match code {
        0 => ("§e", "晴"),
        1 | 2 | 3 => ("§7", "多云"),
        45 | 48 => ("§8", "雾"),
        51 | 53 | 55 | 56 | 57 => ("§9", "小雨"),
        61 | 63 | 65 | 66 | 67 => ("§9", "雨"),
        71 | 73 | 75 | 77 => ("§f", "雪"),
        80 | 81 | 82 => ("§9", "阵雨"),
        85 | 86 => ("§f", "阵雪"),
        95 | 96 | 99 => ("§c", "雷暴"),
        _ => ("§7", "未知"),
    }
}

fn beaufort(kmh: f64) -> u8 {
    const T: [f64; 12] = [1.0, 6.0, 12.0, 20.0, 29.0, 39.0, 50.0, 62.0, 75.0, 89.0, 103.0, 118.0];
    T.iter().filter(|&&t| kmh >= t).count() as u8
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub struct CityWeatherPlugin;

impl Plugin for CityWeatherPlugin {
    fn build(&self, app: &mut App) {
        let (geo_tx, geo_rx) = flume::unbounded::<GeoMsg>();
        let (weather_tx, weather_rx) = flume::unbounded::<(Entity, CityCond)>();
        app.insert_resource(CityNet {
            geo_tx,
            geo_rx,
            weather_tx,
            weather_rx,
        });
        app.add_observer(init_player);
        app.add_systems(
            FixedUpdate,
            (poll_player_weather, drain_city_net, city_hud, wind_particles),
        );
        info!("per-player city weather + wind particles enabled");
    }
}
