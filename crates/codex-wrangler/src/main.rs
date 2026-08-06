mod app;
mod contract;
mod desktop;
mod i3;
mod instance;
mod model;
mod recon;
mod rollout;
mod sigil;
mod transcript;
mod tray;

#[cfg(feature = "egui-test")]
use egui_tester_witness as _;

use anyhow::Result;
use app::Wrangler;
use eternalist_apps::TraceGuard;
use instance::Incumbent;

fn main() -> Result<()> {
    let trace = TraceGuard::arm()?;
    let Some(incumbent) = Incumbent::seize()? else {
        trace.flush();
        return Ok(());
    };
    let ctx = egui::Context::default();
    dwemer_poolrooms::chrome::install(&ctx);
    let result = eternalist_apps::run(ctx.clone(), Wrangler::raise(&ctx, incumbent));
    trace.flush();
    result
}
