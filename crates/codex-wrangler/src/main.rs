mod app;
mod codex_rpc;
mod contract;
mod desktop;
mod i3;
mod instance;
mod model;
mod posture;
mod recon;
mod rollout;
mod roster;
mod sigil;
mod stasis;
mod state;
mod transcript;
mod tray;
mod watchfire;

#[cfg(feature = "egui-test")]
use egui_tester_witness as _;

use anyhow::Result;
use eternalist_apps::TraceGuard;
use instance::Incumbent;
use posture::Ledger;

fn main() -> Result<()> {
    if stasis::thawguard_requested() {
        return stasis::run_thawguard();
    }
    let trace = TraceGuard::arm()?;
    let Some(incumbent) = Incumbent::seize()? else {
        trace.flush();
        return Ok(());
    };
    let ctx = egui::Context::default();
    dwemer_poolrooms::chrome::install(&ctx);
    let (ledger, posture) = Ledger::restore();
    let result = app::launch(&ctx, incumbent, ledger, posture);
    trace.flush();
    result
}
