use std::path::PathBuf;

use bevy::prelude::*;
use hyperion::{runtime::AsyncRuntime, simulation::blocks::Blocks};

pub struct GenMapPlugin;

impl Plugin for GenMapPlugin {
    fn build(&self, app: &mut App) {
        const URL: &str = "https://github.com/andrewgazelka/maps/raw/main/GenMap.tar.gz";

        // Allow overriding the world with a local save directory (must contain `region/`).
        // Lets local dev / custom maps skip the download without re-hosting a tarball.
        let save = if let Ok(dir) = std::env::var("HYPERION_MAP_DIR") {
            PathBuf::from(dir)
        } else {
            let runtime = app
                .world()
                .get_resource::<AsyncRuntime>()
                .expect("AsyncRuntime resource must exist");
            let f = hyperion_utils::cached_save(app.world(), URL);
            runtime.block_on(f).unwrap_or_else(|e| {
                panic!("failed to download map {URL}: {e}");
            })
        };

        let runtime = app
            .world()
            .get_resource::<AsyncRuntime>()
            .expect("AsyncRuntime resource must exist");
        app.insert_resource(Blocks::new(runtime, &save).unwrap());
    }
}
