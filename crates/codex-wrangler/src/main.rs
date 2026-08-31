mod app;
mod codex_rpc;
mod commands;
mod configuration;
mod desktop;
mod fleet;
mod history;
mod i3;
mod instance;
mod integration;
mod model;
mod names;
mod pinboard;
mod posture;
mod recon;
mod relay;
mod rollout;
mod roster;
mod search;
mod site;
mod stasis;
mod state;
mod transcript;
mod tray;
mod watchfire;

#[cfg(feature = "egui-test")]
use egui_tester_witness as _;

use anyhow::Result;
use eternalist_apps::TraceGuard;
use instance::{Incumbent, Invocation};
use posture::Ledger;

fn main() -> Result<()> {
    if matches!(integration::dispatch()?, integration::Dispatch::Exit) {
        return Ok(());
    }
    if stasis::thawguard_requested() {
        return stasis::run_thawguard();
    }
    let invocation = Invocation::read()?;
    let trace = TraceGuard::arm()?;
    let Some(incumbent) = Incumbent::seize(invocation)? else {
        trace.flush();
        return Ok(());
    };
    let ctx = egui::Context::default();
    brass_poolrooms::chrome::install(&ctx);
    let (ledger, posture) = Ledger::restore();
    let result = app::launch(&ctx, incumbent, ledger, posture);
    trace.flush();
    result
}
