use bevy::prelude::*;
use hyperion_clap::MinecraftCommand;

use crate::command::{
    bow::BowCommand, chest::ChestCommand, fly::FlyCommand, gui::GuiCommand,
    raycast::RaycastCommand, setcity::SetCityCommand, shoot::ShootCommand, speed::SpeedCommand,
    vanish::VanishCommand, weather::WeatherCommand, xp::XpCommand,
};

mod bow;
mod chest;
mod fly;
mod gui;
mod raycast;
mod setcity;
mod shoot;
mod speed;
mod vanish;
mod weather;
mod xp;

pub fn register(world: &mut World) {
    BowCommand::register(world);
    FlyCommand::register(world);
    GuiCommand::register(world);
    RaycastCommand::register(world);
    ShootCommand::register(world);
    SpeedCommand::register(world);
    VanishCommand::register(world);
    XpCommand::register(world);
    ChestCommand::register(world);
    WeatherCommand::register(world);
    SetCityCommand::register(world);
}
