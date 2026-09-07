//! Renders the editor states we review by eye. Off by default; set
//! `RELAY_SHOTS=/tmp/relay-shots` to write PNGs there.

use std::sync::Arc;

use relay_plugin::{Mode, RelayParams, editor};
use std::path::Path;

fn shot(name: &str, dir: &str, size: (u32, u32), share: bool, dark: bool, settings_open: bool) {
    let params = Arc::new(RelayParams::default());
    params
        .mode
        .set_value(if share { Mode::Share } else { Mode::Join });
    let mut ed = editor::build(
        Arc::clone(&params),
        size,
        editor::Options {
            dark,
            settings_open,
        },
    );
    let (pixels, w, h) = ed
        .screenshot(params as Arc<dyn truce_params::Params>)
        .expect("headless render");
    truce_core::screenshot::save_png(Path::new(&format!("{dir}/{name}.png")), &pixels, w, h);
}

#[test]
fn render_review_shots() {
    let Ok(dir) = std::env::var("RELAY_SHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    shot("share-dark", &dir, editor::WINDOW, true, true, false);
    shot("join-dark", &dir, editor::WINDOW, false, true, false);
    shot("share-light", &dir, editor::WINDOW, true, false, false);
    shot("settings-dark", &dir, editor::WINDOW, true, true, true);
    shot("share-dark-2x", &dir, (1360, 960), true, true, false);
}
