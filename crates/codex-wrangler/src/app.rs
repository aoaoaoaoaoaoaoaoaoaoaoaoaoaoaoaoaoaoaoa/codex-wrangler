use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(feature = "egui-test")]
use crate::contract::{
    CardKey, CardObservation, CardTarget, Flight, Observation, UI_FINGERPRINT, WorkspaceTarget,
};
use brass_poolrooms::{
    chrome,
    water::{Domain, Floor, Frame as WaterFrame, Poke, Surface, Wetness},
};
use egui::{Color32, RichText, Sense, Stroke, StrokeKind, Vec2};
use eternalist_apps::{CloseDisposition, LivingWait, NativeApp, NativeWake, WindowSpec};

use crate::{
    contract::{Harness, Work},
    instance::{Incumbent, NO_DESKTOP},
    model::{Card, Census},
    posture::{Ledger, Posture},
    recon::{Intent, Nexus, Strike, spawn},
    tray::{Signal as TraySignal, Tray},
};

const TILE_MIN: f32 = 300.0;
const TILE_HEIGHT: f32 = 185.0;
const GAP: f32 = 12.0;
const GREEN: Color32 = Color32::from_rgb(91, 218, 146);
const VIOLET: Color32 = Color32::from_rgb(178, 115, 238);
const ORANGE: Color32 = Color32::from_rgb(235, 158, 74);
const RED: Color32 = Color32::from_rgb(236, 91, 91);
const WHITE: Color32 = Color32::from_rgb(238, 234, 224);
const ASH: Color32 = Color32::from_rgb(174, 172, 166);
const TYPE_LIFT: f32 = 1.0;
const SUMMON_BARRAGE: u8 = 12;
const TILE_AREA_PER_IMPULSE: f32 = 2_000.0;
const TILE_IMPULSE_CEIL: f32 = 1.60;
const TILE_SWEEP_EPSILON: f32 = 0.05;
const FEAR_REACH: f32 = 720.0;
const FEAR_FLEE: f32 = 2.25;

type JoltLedger = HashMap<Harness, HashMap<String, Vec2>>;

struct CardPhysics<'a> {
    jiggling: bool,
    recoiling: bool,
    water: &'a mut Surface,
    jolts: &'a mut JoltLedger,
}

struct ActivationFlight {
    strike: Strike,
    witness: FlightWitness,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FlightWitness {
    Unpresented,
    Presented,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GalleryVisibility {
    Visible,
    ConcealPending,
    Concealed,
}

impl ActivationFlight {
    const fn launch(strike: Strike) -> Self {
        Self {
            strike,
            witness: FlightWitness::Unpresented,
        }
    }

    fn witness(&mut self) {
        self.witness = FlightWitness::Presented;
    }
}

struct Wrangler<const START_FLOATING: bool> {
    water: Surface,
    living_wait: LivingWait,
    nexus: Nexus,
    census: Option<Census>,
    census_label: String,
    first_frame_presented: bool,
    summon: Arc<AtomicU64>,
    summon_generation: u32,
    summon_attempts: Option<u8>,
    posture: Posture,
    pending_activation: Option<ActivationFlight>,
    quit: Arc<AtomicBool>,
    hovered: Option<usize>,
    jiggling: bool,
    jolts: JoltLedger,
    visibility: GalleryVisibility,
    ledger: Ledger,
    tray: Option<Tray>,
}

pub fn launch(
    ctx: &egui::Context,
    incumbent: Incumbent,
    ledger: Ledger,
    posture: Posture,
) -> anyhow::Result<()> {
    match posture {
        Posture::Floating => {
            eternalist_apps::run(ctx.clone(), Wrangler::<true>::raise(ctx, incumbent, ledger))
        }
        Posture::Tiled => eternalist_apps::run(
            ctx.clone(),
            Wrangler::<false>::raise(ctx, incumbent, ledger),
        ),
    }
}

impl<const START_FLOATING: bool> Wrangler<START_FLOATING> {
    fn raise(ctx: &egui::Context, incumbent: Incumbent, ledger: Ledger) -> Self {
        lift_typography(ctx);
        let wake = NativeWake::from_context(ctx);
        let nexus = spawn(wake.clone());
        let summon = Arc::new(AtomicU64::new(pack_summon(1, incumbent.launch_desktop())));
        let tray_summon = Arc::clone(&summon);
        let quit = Arc::new(AtomicBool::new(false));
        let tray_quit = Arc::clone(&quit);
        let tray_wake = wake;
        let tray = Tray::raise(incumbent, move |signal| match signal {
            TraySignal::Reveal(destination) => {
                let destination = match destination {
                    Some(desktop) => Some(desktop),
                    None => match crate::desktop::Desktop::current_desktop() {
                        Ok(desktop) => desktop,
                        Err(error) => {
                            eprintln!(
                                "codex-wrangler could not sight the current workspace: {error:#}"
                            );
                            None
                        }
                    },
                };
                arm_summon(&tray_summon, destination);
                let _woken = tray_wake.wake();
            }
            TraySignal::Quit => {
                tray_quit.store(true, Ordering::Release);
                let _woken = tray_wake.wake();
            }
        })
        .map_err(|error| eprintln!("codex-wrangler could not raise its tray: {error:#}"))
        .ok();
        Self {
            water: Surface::new(Wetness::Wet),
            living_wait: LivingWait::default(),
            nexus,
            census: None,
            census_label: "DISCOVERING MANUAL THREADS".to_owned(),
            first_frame_presented: false,
            summon,
            summon_generation: 0,
            summon_attempts: Some(0),
            posture: Posture::from_floating(START_FLOATING),
            pending_activation: None,
            quit,
            hovered: None,
            jiggling: false,
            jolts: JoltLedger::new(),
            visibility: GalleryVisibility::Visible,
            ledger,
            tray,
        }
    }

    fn quench(&mut self) {
        self.visibility = GalleryVisibility::Concealed;
        self.jiggling = false;
        self.jolts.clear();
        self.water.reset();
        self.water.set_wetness(Wetness::Dry);
    }

    fn request_conceal(&mut self) {
        self.quench();
        self.visibility = GalleryVisibility::ConcealPending;
    }

    fn reap_activation(&mut self) {
        if self
            .pending_activation
            .as_ref()
            .is_some_and(|flight| flight.witness != FlightWitness::Presented)
        {
            return;
        }
        let Some(activation) = self.nexus.take_activation() else {
            return;
        };
        if self
            .pending_activation
            .as_ref()
            .is_none_or(|flight| flight.strike != activation.strike)
        {
            return;
        }
        self.pending_activation = None;
        if activation.succeeded && activation.conceal {
            self.sight_posture();
            if self.posture.floating() {
                self.request_conceal();
            }
        }
    }

    fn kindle_if_summoned(&mut self) -> bool {
        let generation = u32::try_from(self.summon.load(Ordering::Acquire) >> 32)
            .expect("summon generation occupies 32 bits");
        if generation == self.summon_generation {
            return false;
        }
        self.summon_generation = generation;
        self.summon_attempts = Some(0);
        self.visibility = GalleryVisibility::Visible;
        self.water.reset();
        self.water.set_wetness(Wetness::Wet);
        self.sight_posture();
        true
    }

    fn drain(&mut self) -> bool {
        if !self.first_frame_presented {
            return false;
        }
        let Some(census) = self.nexus.take_census() else {
            return false;
        };
        self.census_label = census_count(census.cards.len());
        self.census = Some(census);
        true
    }

    fn sight_posture(&mut self) {
        match crate::desktop::Desktop::process_floating(std::process::id()) {
            Ok(Some(floating)) => {
                self.posture = Posture::from_floating(floating);
                self.ledger.remember(self.posture);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("codex-wrangler could not read its i3 window mode: {error:#}");
            }
        }
    }

    #[cfg(feature = "egui-test")]
    fn observation(&self) -> Observation {
        Observation {
            fingerprint: UI_FINGERPRINT.to_owned(),
            summoning: self.summon_attempts.is_some(),
            hovered: self.hovered.and_then(|index| {
                self.census
                    .as_ref()
                    .and_then(|census| census.cards.get(index))
                    .map(|card| CardKey {
                        harness: card.harness,
                        thread: card.thread.clone(),
                    })
            }),
            loading: self.census.is_none(),
            jiggling: self.jiggling,
            flight: if self.pending_activation.is_some() {
                Flight::Striking
            } else {
                Flight::Grounded
            },
            cards: self
                .census
                .iter()
                .flat_map(|census| &census.cards)
                .map(|card| CardObservation {
                    harness: card.harness,
                    name: card.name.clone(),
                    thread: card.thread.clone(),
                    work: card.work,
                    workspace: card.workspace,
                })
                .collect(),
        }
    }
}

impl<const START_FLOATING: bool> Drop for Wrangler<START_FLOATING> {
    fn drop(&mut self) {
        self.sight_posture();
        self.ledger.remember(self.posture);
    }
}

impl<const START_FLOATING: bool> NativeApp for Wrangler<START_FLOATING> {
    const WINDOW: WindowSpec = if START_FLOATING {
        WindowSpec::new("Codex Wrangler", [1_260.0, 820.0]).floating()
    } else {
        WindowSpec::new("Codex Wrangler", [1_260.0, 820.0])
    };

    fn draw(&mut self, ui: &mut egui::Ui) {
        self.kindle_if_summoned();
        self.hovered = None;
        self.reap_activation();
        if self.visibility != GalleryVisibility::Visible {
            return;
        }
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.sight_posture();
            self.request_conceal();
            return;
        }
        let basin = ui.max_rect();
        let jiggling = ui.input(|input| input.modifiers.shift);
        let recoiling = self.jiggling && !jiggling;
        self.jiggling = jiggling;
        if jiggling {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(45));
        }
        self.water.begin(Domain::basin(basin));
        self.water.set_floor(Some(Floor::shallow(basin)));
        let mut selected = None;
        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(12, 11, 9, 232))
                    .inner_margin(22),
            )
            .show(ui, |ui| {
                let _header = ui.horizontal(|ui| {
                    let _title = ui.label(chrome::title("CODEX WRANGLER").size(18.0));
                    ui.add_space(8.0);
                    let _count = ui.label(chrome::muted(&self.census_label).size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        legend(ui, "DONE", Work::Done);
                        legend(ui, "SLEEP", Work::Sleep);
                        legend(ui, "CLOSED", Work::Closed);
                        legend(ui, "GOAL", Work::Goal);
                        legend(ui, "WORKING", Work::Turn);
                        legend(ui, "INPUT", Work::Input);
                        legend(ui, "ERROR", Work::Error);
                    });
                });
                ui.add_space(16.0);
                let scroll = egui::ScrollArea::vertical()
                    .id_salt("codex-gallery")
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if self.census.is_none() {
                            let arena = ui.max_rect();
                            let _bouncer = self.living_wait.bouncer(ui, arena);
                        } else if let Some(fault) = self
                            .census
                            .as_ref()
                            .and_then(|census| census.fault.as_deref())
                        {
                            let _fault = ui.label(RichText::new(fault).color(Color32::LIGHT_RED));
                        } else if self
                            .census
                            .as_ref()
                            .is_some_and(|census| census.cards.is_empty())
                        {
                            let _empty = ui.centered_and_justified(|ui| {
                                ui.label(
                                    chrome::muted("NO MANUAL HARNESS TERMINALS FOUND").size(13.0),
                                )
                            });
                        } else if let Some(census) = &self.census {
                            selected = gallery(
                                ui,
                                &census.cards,
                                jiggling,
                                recoiling,
                                &mut self.water,
                                &mut self.jolts,
                                &mut self.hovered,
                            );
                        }
                    });
                scroll.state.offset.y
            });
        if !jiggling {
            self.jolts.clear();
        }
        if let Some(strike) = selected
            && self.pending_activation.is_none()
        {
            self.summon_attempts = None;
            self.sight_posture();
            if self.nexus.strike.try_send(strike.clone()).is_ok() {
                self.pending_activation = Some(ActivationFlight::launch(strike));
            }
        }
        self.water.heave(ui.ctx(), panel.inner);
        if self.hovered.is_none() && !jiggling && self.drain() {
            ui.ctx().request_repaint();
        }
    }

    fn close_requested(&mut self) -> CloseDisposition {
        if self.tray.as_ref().is_some_and(Tray::available) {
            self.sight_posture();
            self.quench();
            CloseDisposition::HideOrExit
        } else {
            CloseDisposition::Exit
        }
    }

    fn take_reveal_request(&mut self) -> bool {
        self.kindle_if_summoned()
    }

    fn take_conceal_request(&mut self) -> bool {
        if self.visibility == GalleryVisibility::ConcealPending {
            self.visibility = GalleryVisibility::Concealed;
            true
        } else {
            false
        }
    }

    fn exit_requested(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }

    fn after_present(&mut self) -> bool {
        if let Some(flight) = &mut self.pending_activation {
            flight.witness();
        }
        let first = if self.first_frame_presented {
            false
        } else {
            self.first_frame_presented = true;
            true
        };
        self.kindle_if_summoned();
        let packet = self.summon.load(Ordering::Acquire);
        let Some(attempts) = self.summon_attempts else {
            return first;
        };
        let destination = unpack_desktop(packet);
        match crate::desktop::Desktop::summon_process_to(
            std::process::id(),
            destination,
            self.posture.floating(),
        ) {
            Ok(true) => {
                self.summon_attempts = None;
                true
            }
            Ok(false) | Err(_) if attempts < SUMMON_BARRAGE => {
                self.summon_attempts = Some(attempts + 1);
                true
            }
            Ok(false) => {
                self.summon_attempts = None;
                eprintln!("codex-wrangler could not converge its window summon");
                true
            }
            Err(error) => {
                self.summon_attempts = None;
                eprintln!("codex-wrangler could not summon itself: {error:#}");
                true
            }
        }
    }

    fn water(
        &mut self,
        ctx: &egui::Context,
        pixels_per_point: f32,
        tooltip_rects: &[egui::Rect],
    ) -> WaterFrame {
        if self.visibility == GalleryVisibility::Visible {
            self.living_wait.compose(ctx, &mut self.water);
        }
        self.water.frame(ctx, pixels_per_point, tooltip_rects, None)
    }

    fn register_gpu(
        _renderer: &mut egui_wgpu::Renderer,
        _device: &egui_wgpu::wgpu::Device,
        _format: egui_wgpu::wgpu::TextureFormat,
    ) {
    }

    #[cfg(feature = "egui-test")]
    type Observation = Observation;

    #[cfg(feature = "egui-test")]
    fn observe(&self, _text_edit_focused: bool) -> Self::Observation {
        self.observation()
    }
}

fn gallery(
    ui: &mut egui::Ui,
    cards: &[Card],
    jiggling: bool,
    recoiling: bool,
    water: &mut Surface,
    jolts: &mut JoltLedger,
    hovered: &mut Option<usize>,
) -> Option<Strike> {
    let width = ui.available_width();
    let mut columns = 1_usize;
    let mut column_count = 1.0_f32;
    while (column_count + 1.0) * TILE_MIN + column_count * GAP <= width {
        columns += 1;
        column_count += 1.0;
    }
    let tile_width = ((width - GAP * (column_count - 1.0)) / column_count).max(180.0);
    let mut selected = None;
    let mut physics = CardPhysics {
        jiggling,
        recoiling,
        water,
        jolts,
    };
    for (row_index, row) in cards.chunks(columns).enumerate() {
        let _row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (column, card_model) in row.iter().enumerate() {
                let clicked = card(
                    ui,
                    card_model,
                    row_index * columns + column,
                    tile_width,
                    &mut physics,
                    hovered,
                );
                if selected.is_none() {
                    selected = clicked;
                }
            }
        });
        ui.add_space(GAP);
    }
    selected
}

fn card(
    ui: &mut egui::Ui,
    card: &Card,
    index: usize,
    width: f32,
    physics: &mut CardPhysics<'_>,
    hovered_card: &mut Option<usize>,
) -> Option<Strike> {
    let (id, rect) = ui.allocate_space(Vec2::new(width, TILE_HEIGHT));
    let dismissible = dismissible(card.harness, card.work);
    let fleeing = physics.jiggling && dismissible;
    let offset = fear_offset(ui, &card.thread, rect, fleeing);
    let visual = rect.translate(offset);
    let pointer_inside = ui.rect_contains_pointer(visual);
    if physics.jiggling || physics.recoiling {
        let travel = advance_jolt(
            physics.jolts,
            card.harness,
            &card.thread,
            fleeing.then_some(offset),
        );
        displace_tile(physics.water, visual, travel);
    }
    let stroke = if pointer_inside {
        Stroke::new(1.5_f32, chrome::EDGE_STRONG)
    } else {
        Stroke::new(1.0_f32, chrome::EDGE)
    };
    let fill = if pointer_inside {
        Color32::from_rgba_unmultiplied(39, 32, 24, 226)
    } else {
        Color32::from_rgba_unmultiplied(19, 16, 13, 218)
    };
    ui.painter().rect_filled(visual, 2, fill);
    ui.painter()
        .rect_stroke(visual, 2, stroke, StrokeKind::Inside);
    paint_card_contents(ui, visual, card);

    // Final authority owns the whole tile after every inert child.
    let response = ui
        .interact(visual, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    brass_poolrooms::poolroom_anchor!(
        ui,
        CardTarget(card.harness, &card.thread).to_string(),
        response.rect
    );
    if response.hovered() {
        *hovered_card = Some(index);
        physics
            .water
            .hover((card.harness.slug(), &card.thread), visual);
    }
    if response.clicked() {
        if physics.jiggling && !dismissible {
            return None;
        }
        physics.water.click(visual);
        let intent = if physics.jiggling {
            Intent::Dismiss
        } else {
            Intent::Select
        };
        Some(Strike {
            harness: card.harness,
            thread: card.thread.clone(),
            intent,
        })
    } else {
        None
    }
}

fn paint_card_contents(ui: &mut egui::Ui, visual: egui::Rect, card: &Card) {
    let workspace_width = paint_workspace(ui, visual, card);
    let inner = visual.shrink2(Vec2::new(14.0, 11.0));
    let mut body = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("harness-card-body", card.harness, &card.thread))
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    body.set_max_width(inner.width());
    body.set_clip_rect(ui.clip_rect().intersect(inner));
    let name = card.name.as_deref();
    let name_text = name.map_or_else(
        || RichText::new("anonymous").small().color(chrome::MUTED),
        |name| RichText::new(name).strong().color(chrome::TEXT),
    );
    body.set_max_width((inner.width() - workspace_width).max(80.0));
    let _name = body.add(
        egui::Label::new(name_text)
            .truncate()
            .show_tooltip_when_elided(false),
    );
    body.set_max_width(inner.width());
    body.add_space(6.0);
    let _cwd = body.add(
        egui::Label::new(RichText::new(&card.cwd).color(chrome::HOT))
            .truncate()
            .show_tooltip_when_elided(false),
    );
    body.add_space(8.0);
    let tile_preview = if card.tile_preview.is_empty() {
        "No conversational turn recorded."
    } else {
        &card.tile_preview
    };
    let _preview = body.add(
        egui::Label::new(RichText::new(tile_preview).color(chrome::MUTED))
            .wrap()
            .show_tooltip_when_elided(false),
    );
    drop(body);
    paint_card_work(ui.painter(), visual, card.work);
}

fn fear_offset(ui: &egui::Ui, thread: &str, bounds: egui::Rect, afraid: bool) -> Vec2 {
    if !afraid {
        return Vec2::ZERO;
    }
    let (clock, pointer) = ui.input(|input| (input.time, input.pointer.hover_pos()));
    fear_pose(thread, clock, bounds, pointer)
}

fn fear_pose(thread: &str, clock: f64, bounds: egui::Rect, pointer: Option<egui::Pos2>) -> Vec2 {
    let Some(pointer) = pointer else {
        return Vec2::ZERO;
    };
    let away = bounds.center() - pointer;
    let proximity = fear_proximity(away.length());
    let direction = if away.length_sq() > f32::EPSILON {
        away.normalized()
    } else {
        let fallback = tremor(thread, 0.0);
        if fallback.length_sq() > f32::EPSILON {
            fallback.normalized()
        } else {
            Vec2::X
        }
    };
    tremor(thread, clock) * proximity + direction * (FEAR_FLEE * proximity)
}

const fn dismissible(harness: Harness, work: Work) -> bool {
    matches!(harness, Harness::Codex) && matches!(work, Work::Done | Work::Sleep | Work::Closed)
}

fn fear_proximity(distance: f32) -> f32 {
    let linear = (1.0 - distance / FEAR_REACH).clamp(0.0, 1.0);
    linear * linear * (3.0 - 2.0 * linear)
}

fn tremor(thread: &str, time: f64) -> Vec2 {
    const AXIS: [f32; 9] = [-2.0, -1.5, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5, 2.0];
    let millis = std::time::Duration::from_secs_f64(time.max(0.0)).as_millis();
    let tick = u64::try_from(millis / 59).unwrap_or(u64::MAX);
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ tick.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for byte in thread.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let component = |bits: u64| AXIS[usize::try_from(bits % 9).expect("jolt axis is bounded")];
    egui::vec2(component(hash), component(hash.rotate_left(29)))
}

fn advance_jolt(
    ledger: &mut JoltLedger,
    harness: Harness,
    thread: &str,
    current: Option<Vec2>,
) -> Vec2 {
    let bank = ledger.entry(harness).or_default();
    match current {
        Some(current) => {
            if let Some(prior) = bank.get_mut(thread) {
                let travel = current - *prior;
                *prior = current;
                travel
            } else {
                let _vacant = bank.insert(thread.to_owned(), current);
                current
            }
        }
        None => bank.remove(thread).map_or(Vec2::ZERO, |prior| -prior),
    }
}

/// Couple the tile's projected swept volume into both water axes. `rect` is
/// the current visual pose; layout motion is deliberately excluded because
/// tray heave already owns it.
fn displace_tile(water: &mut Surface, rect: egui::Rect, travel: Vec2) {
    let envelope = rect.union(rect.translate(-travel));
    let horizontal = travel.x.abs() * rect.height();
    if horizontal >= TILE_SWEEP_EPSILON {
        water.poke(
            envelope,
            Poke::slide(
                (horizontal / TILE_AREA_PER_IMPULSE).min(TILE_IMPULSE_CEIL),
                travel.x.signum(),
            ),
        );
    }
    let vertical = travel.y.abs() * rect.width();
    if vertical >= TILE_SWEEP_EPSILON {
        water.poke(
            envelope,
            Poke::drag(
                (vertical / TILE_AREA_PER_IMPULSE).min(TILE_IMPULSE_CEIL),
                travel.y.signum(),
            ),
        );
    }
}

fn legend(ui: &mut egui::Ui, label: &str, work: Work) {
    let _legend = ui.horizontal(|ui| {
        let (rect, _response) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        paint_work(ui.painter(), rect.center(), 3.25, work);
        let _label = ui.label(RichText::new(label).small().color(chrome::MUTED));
    });
}

fn paint_card_work(painter: &egui::Painter, tile: egui::Rect, work: Work) {
    paint_work(
        painter,
        tile.right_bottom() - egui::vec2(13.0, 13.0),
        4.5,
        work,
    );
}

fn paint_closed(painter: &egui::Painter, center: egui::Pos2, radius: f32) {
    let points = vec![
        center + egui::vec2(0.0, -radius),
        center + egui::vec2(radius, radius),
        center + egui::vec2(-radius, radius),
    ];
    painter.add(egui::Shape::convex_polygon(
        points,
        Color32::BLACK,
        Stroke::new(1.0_f32, chrome::MUTED),
    ));
}

fn paint_work(painter: &egui::Painter, center: egui::Pos2, radius: f32, work: Work) {
    if work == Work::Closed {
        paint_closed(painter, center, radius + 0.75);
        return;
    }
    let color = work_color(work);
    match work {
        Work::Goal | Work::Turn => {
            painter.circle_filled(center, radius, color);
        }
        Work::Error | Work::Input | Work::Sleep | Work::Done => {
            painter.rect_filled(
                egui::Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
                0,
                color,
            );
        }
        Work::Closed => unreachable!("closed returns before square projection"),
    }
}

fn paint_workspace(ui: &egui::Ui, tile: egui::Rect, card: &Card) -> f32 {
    let Some(workspace) = card.workspace else {
        return 0.0;
    };
    let galley = ui.painter().layout_no_wrap(
        workspace.to_string(),
        egui::FontId::new(14.0, egui::FontFamily::Monospace),
        chrome::TEXT,
    );
    let height = (galley.size().y + 6.0).max(25.0);
    let width = (galley.size().x + 12.0).max(25.0);
    let edge = Stroke::new(1.0_f32, chrome::EDGE_STRONG);
    let rect = egui::Rect::from_min_size(
        egui::pos2(tile.right() - width, tile.top()),
        egui::vec2(width, height),
    );
    let radius = egui::CornerRadius {
        nw: 2,
        ne: 2,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(rect, radius, chrome::RAISED);
    ui.painter()
        .rect_stroke(rect, radius, edge, StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, chrome::TEXT);
    brass_poolrooms::poolroom_anchor!(
        ui,
        WorkspaceTarget(card.harness, &card.thread).to_string(),
        rect
    );
    width
}

const fn work_color(work: Work) -> Color32 {
    match work {
        Work::Error => RED,
        Work::Input => ORANGE,
        Work::Goal => VIOLET,
        Work::Turn => GREEN,
        Work::Sleep => ASH,
        Work::Done => WHITE,
        Work::Closed => Color32::BLACK,
    }
}

fn census_count(count: usize) -> String {
    let noun = if count == 1 { "THREAD" } else { "THREADS" };
    format!("{count} MANUAL {noun}")
}

fn lift_typography(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    for font in style.text_styles.values_mut() {
        font.size += TYPE_LIFT;
    }
    ctx.set_global_style(style);
}

fn arm_summon(summon: &AtomicU64, destination: Option<u32>) {
    let prior = summon.load(Ordering::Relaxed);
    let generation = u32::try_from(prior >> 32)
        .expect("summon generation occupies 32 bits")
        .wrapping_add(1);
    summon.store(pack_summon(generation, destination), Ordering::Release);
}

const fn pack_summon(generation: u32, destination: Option<u32>) -> u64 {
    let desktop = match destination {
        Some(desktop) => desktop,
        None => NO_DESKTOP,
    };
    (generation as u64) << 32 | desktop as u64
}

fn unpack_desktop(packet: u64) -> Option<u32> {
    let desktop =
        u32::try_from(packet & u64::from(u32::MAX)).expect("summon desktop occupies 32 bits");
    (desktop != NO_DESKTOP).then_some(desktop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_palette_is_exact() {
        assert_eq!(work_color(Work::Turn), GREEN);
        assert_eq!(work_color(Work::Goal), VIOLET);
        assert_eq!(work_color(Work::Input), ORANGE);
        assert_eq!(work_color(Work::Error), RED);
        assert_eq!(work_color(Work::Sleep), ASH);
        assert_eq!(work_color(Work::Closed), Color32::BLACK);
        assert_eq!(work_color(Work::Done), WHITE);
    }

    #[test]
    fn typography_rises_by_exactly_one_point() {
        let ctx = egui::Context::default();
        let before = ctx.global_style().text_styles.clone();
        lift_typography(&ctx);
        for (text_style, font) in &ctx.global_style().text_styles {
            assert!((font.size - before[text_style].size - 1.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn restored_posture_selects_the_native_window_species() {
        const {
            assert!(<Wrangler<true> as NativeApp>::WINDOW.floating);
            assert!(!<Wrangler<false> as NativeApp>::WINDOW.floating);
        }
    }

    #[test]
    fn fear_is_near_loud_far_quiet_and_repulsed() {
        let tile = egui::Rect::from_center_size(egui::pos2(500.0, 400.0), egui::vec2(300.0, 185.0));
        let near_pointer = egui::pos2(490.0, 400.0);
        let far_pointer = egui::pos2(-100.0, 400.0);
        let samples = (0..20)
            .map(|tick| fear_pose("thread", f64::from(tick) / 17.0, tile, Some(near_pointer)))
            .collect::<Vec<_>>();
        let far = (0..20)
            .map(|tick| fear_pose("thread", f64::from(tick) / 17.0, tile, Some(far_pointer)))
            .collect::<Vec<_>>();
        assert!(samples.windows(2).any(|pair| pair[0] != pair[1]));
        assert!(motion_span(&samples) > motion_span(&far));
        assert!(samples.iter().all(|sample| sample.x > 0.0));
        assert_eq!(fear_pose("thread", 0.0, tile, None), Vec2::ZERO);
    }

    #[test]
    fn only_stopped_tiles_fear_management() {
        assert!(dismissible(Harness::Codex, Work::Done));
        assert!(dismissible(Harness::Codex, Work::Sleep));
        assert!(dismissible(Harness::Codex, Work::Closed));
        assert!(!dismissible(Harness::Codex, Work::Error));
        assert!(!dismissible(Harness::Codex, Work::Input));
        assert!(!dismissible(Harness::Codex, Work::Goal));
        assert!(!dismissible(Harness::Codex, Work::Turn));
        assert!(!dismissible(Harness::ClaudeCode, Work::Done));
        assert!(!dismissible(Harness::PrimeAgent, Work::Sleep));
    }

    #[test]
    fn jiggle_sweeps_actual_tile_volume_into_water() {
        let ctx = egui::Context::default();
        let mut water = Surface::new(Wetness::Wet);
        let rect = egui::Rect::from_min_size(egui::pos2(40.0, 50.0), egui::vec2(300.0, 185.0));
        displace_tile(&mut water, rect, egui::vec2(2.0, -1.5));
        water.begin(Domain::basin(rect.expand(100.0)));
        let frame = water.frame(&ctx, 1.0, &[], None);
        assert!(frame.wants_repaint());
    }

    fn motion_span(samples: &[Vec2]) -> f32 {
        samples
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).length())
            .fold(0.0, f32::max)
    }
}
