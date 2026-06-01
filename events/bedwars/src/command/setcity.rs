use bevy::{ecs::system::SystemState, prelude::*};
use clap::Parser;
use hyperion::{
    net::{Compose, ConnectionId, agnostic},
    runtime::AsyncRuntime,
};
use hyperion_clap::{CommandPermission, MinecraftCommand};
use tracing::warn;

use crate::plugin::cityweather::CityNet;

/// `/setcity <城市名>` — show your own city's weather on the action bar.
#[derive(Parser, CommandPermission, Debug)]
#[command(name = "setcity")]
#[command_permission(group = "Normal")]
pub struct SetCityCommand {
    /// City name (may contain spaces / non-ASCII).
    name: Vec<String>,
}

impl MinecraftCommand for SetCityCommand {
    type State = SystemState<()>;

    fn execute(self, world: &World, _state: &mut Self::State, caller: Entity) {
        let compose = world.resource::<Compose>();
        let Some(&connection) = world.entity(caller).get::<ConnectionId>() else {
            return;
        };

        let name = self.name.join(" ");
        if name.trim().is_empty() {
            let _ok = compose.unicast(&agnostic::chat("§c用法:/setcity <城市名>"), connection);
            return;
        }

        let runtime = world.resource::<AsyncRuntime>();
        let tx = world.resource::<CityNet>().geo_tx.clone();
        let query = name.clone();
        runtime.spawn(async move {
            match geocode(&query).await {
                Some((lat, lon, resolved)) => {
                    let _ok = tx.send_async((caller, lat, lon, resolved)).await;
                }
                None => warn!("geocode: no result for {query}"),
            }
        });

        let _ok = compose.unicast(
            &agnostic::chat(format!("§e正在查询 §f{name} §e的天气…")),
            connection,
        );
    }
}

/// Resolve a place name to (lat, lon, canonical name) via open-meteo geocoding.
async fn geocode(name: &str) -> Option<(f64, f64, String)> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh",
        urlencode(name)
    );
    let body = hyperion_utils::fetch_text(&url).await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let r = json["results"].get(0)?;
    let lat = r["latitude"].as_f64()?;
    let lon = r["longitude"].as_f64()?;
    let resolved = r["name"].as_str().unwrap_or(name).to_string();
    Some((lat, lon, resolved))
}

/// Minimal percent-encoding for a URL query value (handles spaces + UTF-8).
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
