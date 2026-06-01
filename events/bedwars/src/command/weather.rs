use bevy::{ecs::system::SystemState, prelude::*};
use clap::Parser;
use hyperion::net::{Compose, ConnectionId, DataBundle, agnostic};
use hyperion_clap::{CommandPermission, MinecraftCommand};
use tracing::error;

use crate::plugin::weather::{Weather, WeatherKind};

/// `/weather <storm|quake|calm|persist> [arg]`
/// - `storm [1-12]` / `quake [1-12]` — start an event (default level 12 / 10)
/// - `calm` — stop the current event
/// - `persist <on|off>` — toggle whether weather damage is permanent
#[derive(Parser, CommandPermission, Debug)]
#[command(name = "weather")]
#[command_permission(group = "Moderator")]
pub struct WeatherCommand {
    action: String,
    arg: Option<String>,
}

/// What the command resolved to; applied to the [`Weather`] resource via a
/// deferred command (the command handler only gets `&World`).
#[derive(Clone, Copy)]
enum Apply {
    Start(WeatherKind, u8, u32),
    Calm,
    Persist(bool),
    Auto(bool),
    Noop,
}

impl MinecraftCommand for WeatherCommand {
    type State = SystemState<(
        Res<'static, Compose>,
        Query<'static, 'static, &'static ConnectionId>,
        Commands<'static, 'static>,
    )>;

    fn execute(self, world: &World, state: &mut Self::State, caller: Entity) {
        let (compose, query, mut commands) = state.get(world);

        let Ok(&connection) = query.get(caller) else {
            error!("weather command: caller has no ConnectionId");
            return;
        };

        let int_arg = |default: u8| {
            self.arg
                .as_deref()
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(default)
                .clamp(1, 12)
        };

        let (apply, reply) = match self.action.as_str() {
            "storm" => {
                let lvl = int_arg(12);
                (
                    Apply::Start(WeatherKind::Storm, lvl, 60),
                    format!("§c⚡ Summoned a level {lvl} storm (60s)"),
                )
            }
            "quake" => {
                let lvl = int_arg(10);
                (
                    Apply::Start(WeatherKind::Quake, lvl, 30),
                    format!("§6☶ Summoned a level {lvl} quake (30s)"),
                )
            }
            "calm" => (Apply::Calm, "§aThe weather has calmed.".to_string()),
            "persist" => {
                let on = matches!(self.arg.as_deref(), Some("on" | "true" | "1"));
                let msg = if on {
                    "§eDamage mode: §cpermanent §e(ruins persist across restart)"
                } else {
                    "§eDamage mode: §arestoring §e(world heals on restart)"
                };
                (Apply::Persist(on), msg.to_string())
            }
            "auto" => {
                let on = matches!(self.arg.as_deref(), Some("on" | "true" | "1"));
                let msg = if on {
                    "§eEnabled §areal-world weather follow §e(polled every 5 min)"
                } else {
                    "§eDisabled real-world weather follow §7(manual commands only)"
                };
                (Apply::Auto(on), msg.to_string())
            }
            other => (
                Apply::Noop,
                format!("§cUnknown usage: {other}. Available: storm|quake|calm|persist|auto"),
            ),
        };

        // Resource mutation is deferred (we only have &World here).
        commands.queue(move |world: &mut World| {
            let mut weather = world.resource_mut::<Weather>();
            match apply {
                Apply::Start(kind, lvl, secs) => weather.start(kind, lvl, secs),
                Apply::Calm => weather.calm(),
                Apply::Persist(on) => weather.persist_damage = on,
                Apply::Auto(on) => weather.auto_follow = on,
                Apply::Noop => {}
            }
        });

        let mut bundle = DataBundle::new(&compose);
        if bundle.add_packet(&agnostic::chat(reply)).is_ok() {
            let _ok = bundle.unicast(connection);
        }
    }
}
