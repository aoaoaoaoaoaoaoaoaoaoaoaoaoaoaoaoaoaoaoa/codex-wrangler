use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(feature = "egui-test")]
use crate::contract::{
    CardKey, CardObservation, CardTarget, LogoTarget, Observation, UI_FINGERPRINT, WorkspaceTarget,
};
use dwemer_poolrooms::{
    chrome,
    water::{Domain, Floor, Frame as WaterFrame, Surface, Wetness},
};
use egui::{Color32, RichText, Sense, Stroke, StrokeKind, Vec2};
use eternalist_apps::{CloseDisposition, LivingWait, NativeApp, WindowSpec};

use crate::{
    contract::{Harness, Work},
    instance::{Incumbent, NO_DESKTOP},
    model::{Card, Census},
    recon::{Nexus, Strike, spawn},
    sigil,
    tray::{Signal as TraySignal, Tray},
};

const TILE_MIN: f32 = 300.0;
const TILE_HEIGHT: f32 = 185.0;
const GAP: f32 = 12.0;
const GREEN: Color32 = Color32::from_rgb(91, 218, 146);
const VIOLET: Color32 = Color32::from_rgb(178, 115, 238);
const RED: Color32 = Color32::from_rgb(236, 91, 91);
const WHITE: Color32 = Color32::from_rgb(238, 234, 224);
const TYPE_LIFT: f32 = 1.0;
const SUMMON_BARRAGE: u8 = 12;

pub struct Wrangler {
    water: Surface,
    living_wait: LivingWait,
    nexus: Nexus,
    census: Option<Census>,
    census_label: String,
    first_frame_presented: bool,
    void_sighted: bool,
    summon: Arc<AtomicU64>,
    summon_generation: u32,
    summon_attempts: Option<u8>,
    posture: Posture,
    quit: Arc<AtomicBool>,
    hovered: Option<usize>,
    tray: Option<Tray>,
}

#[derive(Clone, Copy)]
enum Posture {
    Floating,
    Tiled,
}

impl Posture {
    const fn from_floating(floating: bool) -> Self {
        if floating {
            Self::Floating
        } else {
            Self::Tiled
        }
    }

    const fn floating(self) -> bool {
        matches!(self, Self::Floating)
    }
}

impl Wrangler {
    pub fn raise(ctx: &egui::Context, incumbent: Incumbent) -> Self {
        lift_typography(ctx);
        let nexus = spawn(ctx.clone());
        let summon = Arc::new(AtomicU64::new(pack_summon(1, incumbent.launch_desktop())));
        let tray_summon = Arc::clone(&summon);
        let quit = Arc::new(AtomicBool::new(false));
        let tray_quit = Arc::clone(&quit);
        let awaken = ctx.clone();
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
                awaken.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                awaken.request_repaint();
            }
            TraySignal::Quit => {
                tray_quit.store(true, Ordering::Release);
                awaken.request_repaint();
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
            void_sighted: false,
            summon,
            summon_generation: 0,
            summon_attempts: Some(0),
            posture: Posture::from_floating(Self::WINDOW.floating),
            quit,
            hovered: None,
            tray,
        }
    }

    fn drain(&mut self) -> bool {
        if !self.first_frame_presented {
            return false;
        }
        let mut changed = false;
        for census in self.nexus.census.try_iter() {
            if let Some(census) =
                admit_census(self.census.is_some(), &mut self.void_sighted, census)
            {
                self.census_label = census_count(census.cards.len());
                self.census = Some(census);
                changed = true;
            }
        }
        changed
    }

    fn sight_posture(&mut self) {
        match crate::desktop::Desktop::process_floating(std::process::id()) {
            Ok(Some(floating)) => self.posture = Posture::from_floating(floating),
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

impl NativeApp for Wrangler {
    const WINDOW: WindowSpec = WindowSpec::new("Codex Wrangler", [1_260.0, 820.0]).floating();

    fn draw(&mut self, ui: &mut egui::Ui) {
        self.hovered = None;
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.sight_posture();
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let basin = ui.max_rect();
        self.water.begin(Domain::basin(basin));
        self.water.set_floor(Some(Floor::shallow(basin)));
        let mut selected = None;
        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(12, 11, 9, 232))
                    .inner_margin(22),
            )
            .show_inside(ui, |ui| {
                let _header = ui.horizontal(|ui| {
                    let _title = ui.label(chrome::title("CODEX WRANGLER").size(18.0));
                    ui.add_space(8.0);
                    let _count = ui.label(chrome::muted(&self.census_label).size(13.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        legend(ui, "DONE", Work::Done);
                        legend(ui, "GOAL", Work::Goal);
                        legend(ui, "WORKING", Work::Turn);
                        legend(ui, "INPUT", Work::Input);
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
                            selected =
                                gallery(ui, &census.cards, &mut self.water, &mut self.hovered);
                        }
                    });
                scroll.state.offset.y
            });
        if let Some(window) = selected {
            self.summon_attempts = None;
            self.sight_posture();
            let _sent = self.nexus.strike.try_send(Strike::Activate(window));
            if self.posture.floating() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        self.water.heave(ui.ctx(), panel.inner);
        if self.hovered.is_none() && self.drain() {
            ui.ctx().request_repaint();
        }
    }

    fn close_requested(&mut self) -> CloseDisposition {
        if self.tray.as_ref().is_some_and(Tray::available) {
            self.sight_posture();
            CloseDisposition::Hide
        } else {
            CloseDisposition::Exit
        }
    }

    fn exit_requested(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }

    fn after_present(&mut self) -> bool {
        let first = if self.first_frame_presented {
            false
        } else {
            self.first_frame_presented = true;
            true
        };
        let packet = self.summon.load(Ordering::Acquire);
        let generation = u32::try_from(packet >> 32).expect("summon generation occupies 32 bits");
        if generation != self.summon_generation {
            self.summon_generation = generation;
            self.summon_attempts = Some(0);
            self.sight_posture();
        }
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
        self.living_wait.compose(ctx, &mut self.water);
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
    water: &mut Surface,
    hovered: &mut Option<usize>,
) -> Option<u32> {
    let width = ui.available_width();
    let mut columns = 1_usize;
    let mut column_count = 1.0_f32;
    while (column_count + 1.0) * TILE_MIN + column_count * GAP <= width {
        columns += 1;
        column_count += 1.0;
    }
    let tile_width = ((width - GAP * (column_count - 1.0)) / column_count).max(180.0);
    let mut selected = None;
    for (row_index, row) in cards.chunks(columns).enumerate() {
        let _row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (column, card_model) in row.iter().enumerate() {
                let clicked = card(
                    ui,
                    card_model,
                    row_index * columns + column,
                    tile_width,
                    water,
                    hovered,
                );
                selected = selected.or(clicked);
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
    water: &mut Surface,
    hovered_card: &mut Option<usize>,
) -> Option<u32> {
    let (id, rect) = ui.allocate_space(Vec2::new(width, TILE_HEIGHT));
    let pointer_inside = ui.rect_contains_pointer(rect);
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
    ui.painter().rect_filled(rect, 2, fill);
    ui.painter()
        .rect_stroke(rect, 2, stroke, StrokeKind::Inside);
    let badge_width = paint_harness_badges(ui, rect, card.harness, &card.thread, card.workspace);

    let inner = rect.shrink2(Vec2::new(14.0, 11.0));
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
    body.set_max_width((inner.width() - badge_width).max(80.0));
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
    paint_work(
        ui.painter(),
        rect.right_bottom() - egui::vec2(13.0, 13.0),
        4.5,
        card.work,
    );

    // This interaction is deliberately registered after every inert child.
    // One final authority owns the entire rectangle, including text pixels.
    let response = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    dwemer_poolrooms::poolroom_anchor!(
        ui,
        CardTarget(card.harness, &card.thread).to_string(),
        response.rect
    );
    if response.hovered() {
        *hovered_card = Some(index);
        water.hover((card.harness.slug(), &card.thread), rect);
    }
    if response.clicked() {
        water.click(rect);
        Some(card.window)
    } else {
        None
    }
}

fn legend(ui: &mut egui::Ui, label: &str, work: Work) {
    let _legend = ui.horizontal(|ui| {
        let (rect, _response) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        paint_work(ui.painter(), rect.center(), 3.25, work);
        let _label = ui.label(RichText::new(label).small().color(chrome::MUTED));
    });
}

fn paint_work(painter: &egui::Painter, center: egui::Pos2, radius: f32, work: Work) {
    let color = work_color(work);
    match work {
        Work::Goal | Work::Turn => {
            painter.circle_filled(center, radius, color);
        }
        Work::Input | Work::Done => {
            painter.rect_filled(
                egui::Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
                0,
                color,
            );
        }
    }
}

fn paint_harness_badges(
    ui: &egui::Ui,
    tile: egui::Rect,
    harness: Harness,
    thread: &str,
    workspace: Option<u32>,
) -> f32 {
    #[cfg(not(feature = "instrumentation"))]
    let _ = thread;
    let galley = workspace.map(|workspace| {
        ui.painter().layout_no_wrap(
            workspace.to_string(),
            egui::FontId::new(14.0, egui::FontFamily::Monospace),
            chrome::TEXT,
        )
    });
    let height = galley
        .as_ref()
        .map_or(25.0, |galley| (galley.size().y + 6.0).max(25.0));
    let workspace_width = galley
        .as_ref()
        .map_or(0.0, |galley| (galley.size().x + 12.0).max(25.0));
    let edge = Stroke::new(1.0_f32, chrome::EDGE_STRONG);
    if let Some(galley) = galley {
        let rect = egui::Rect::from_min_size(
            egui::pos2(tile.right() - workspace_width, tile.top()),
            egui::vec2(workspace_width, height),
        );
        let radius = egui::CornerRadius {
            nw: 0,
            ne: 2,
            sw: 0,
            se: 0,
        };
        ui.painter().rect_filled(rect, radius, chrome::RAISED);
        ui.painter()
            .rect_stroke(rect, radius, edge, StrokeKind::Inside);
        ui.painter()
            .galley(rect.center() - galley.size() * 0.5, galley, chrome::TEXT);
        dwemer_poolrooms::poolroom_anchor!(ui, WorkspaceTarget(harness, thread).to_string(), rect);
    }
    let logo_width = 28.0;
    let logo = egui::Rect::from_min_size(
        egui::pos2(tile.right() - workspace_width - logo_width, tile.top()),
        egui::vec2(logo_width, height),
    );
    let radius = egui::CornerRadius {
        nw: 2,
        ne: u8::from(workspace.is_none()) * 2,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(logo, radius, chrome::RAISED);
    ui.painter()
        .rect_stroke(logo, radius, edge, StrokeKind::Inside);
    sigil::paint(ui.painter(), logo.shrink(2.0), harness);
    dwemer_poolrooms::poolroom_anchor!(ui, LogoTarget(harness, thread).to_string(), logo);
    logo_width + workspace_width
}

const fn work_color(work: Work) -> Color32 {
    match work {
        Work::Input => RED,
        Work::Goal => VIOLET,
        Work::Turn => GREEN,
        Work::Done => WHITE,
    }
}

fn census_count(count: usize) -> String {
    let noun = if count == 1 { "THREAD" } else { "THREADS" };
    format!("{count} MANUAL {noun}")
}

fn admit_census(settled: bool, void_sighted: &mut bool, census: Census) -> Option<Census> {
    let unconfirmed_void = !settled
        && census.fault.is_none()
        && census.cards.is_empty()
        && !std::mem::replace(void_sighted, true);
    if unconfirmed_void { None } else { Some(census) }
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
        assert_eq!(work_color(Work::Input), RED);
        assert_eq!(work_color(Work::Done), WHITE);
    }

    #[test]
    fn the_initial_void_must_survive_two_censuses() {
        let mut void_sighted = false;
        assert_eq!(
            admit_census(false, &mut void_sighted, Census::default()),
            None
        );
        assert_eq!(
            admit_census(false, &mut void_sighted, Census::default()),
            Some(Census::default())
        );
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
}
