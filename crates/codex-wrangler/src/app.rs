use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

#[cfg(feature = "egui-test")]
use crate::contract::{CardObservation, CardTarget, Observation, UI_FINGERPRINT};
use dwemer_poolrooms::{
    chrome,
    water::{Domain, Floor, Frame as WaterFrame, Surface, Wetness},
};
use egui::{Color32, RichText, Sense, Stroke, StrokeKind, Vec2};
use eternalist_apps::{CloseDisposition, LivingWait, NativeApp, WindowSpec};

use crate::{
    contract::Work,
    instance::{Incumbent, NO_DESKTOP},
    model::{Census, CodexCard},
    recon::{Nexus, Strike, spawn},
    tray::{Signal as TraySignal, Tray},
};

const TILE_MIN: f32 = 300.0;
const TILE_HEIGHT: f32 = 185.0;
const GAP: f32 = 12.0;
const GREEN: Color32 = Color32::from_rgb(91, 218, 146);
const VIOLET: Color32 = Color32::from_rgb(178, 115, 238);
const WHITE: Color32 = Color32::from_rgb(238, 234, 224);
const TYPE_LIFT: f32 = 1.0;

pub struct Wrangler {
    water: Surface,
    living_wait: LivingWait,
    nexus: Nexus,
    census: Option<Census>,
    census_label: String,
    first_frame_presented: bool,
    void_sighted: bool,
    summon: Arc<AtomicU32>,
    quit: Arc<AtomicBool>,
    hovered: Option<usize>,
    tray: Option<Tray>,
}

impl Wrangler {
    pub fn raise(ctx: &egui::Context, incumbent: Incumbent) -> Self {
        lift_typography(ctx);
        let nexus = spawn(ctx.clone());
        let summon = Arc::new(AtomicU32::new(NO_DESKTOP));
        let tray_summon = Arc::clone(&summon);
        let quit = Arc::new(AtomicBool::new(false));
        let tray_quit = Arc::clone(&quit);
        let awaken = ctx.clone();
        let tray = Tray::raise(incumbent, move |signal| match signal {
            TraySignal::Reveal(destination) => {
                let armed = match destination {
                    Some(desktop) => {
                        tray_summon.store(desktop, Ordering::Release);
                        true
                    }
                    None => match crate::desktop::Desktop::current_desktop() {
                        Ok(Some(desktop)) => {
                            tray_summon.store(desktop, Ordering::Release);
                            true
                        }
                        Ok(None) => false,
                        Err(error) => {
                            eprintln!(
                                "codex-wrangler could not sight the current workspace: {error:#}"
                            );
                            false
                        }
                    },
                };
                awaken.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                if !armed {
                    awaken.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
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
            quit,
            hovered: None,
            tray,
        }
    }

    fn drain(&mut self) {
        if !self.first_frame_presented {
            return;
        }
        for census in self.nexus.census.try_iter() {
            if let Some(census) =
                admit_census(self.census.is_some(), &mut self.void_sighted, census)
            {
                self.census_label = census_count(census.cards.len());
                self.census = Some(census);
            }
        }
    }

    #[cfg(feature = "egui-test")]
    fn observation(&self) -> Observation {
        Observation {
            fingerprint: UI_FINGERPRINT.to_owned(),
            hovered: self.hovered.and_then(|index| {
                self.census
                    .as_ref()
                    .and_then(|census| census.cards.get(index))
                    .map(|card| card.thread.clone())
            }),
            loading: self.census.is_none(),
            cards: self
                .census
                .iter()
                .flat_map(|census| &census.cards)
                .map(|card| CardObservation {
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
        self.drain();
        self.hovered = None;
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let basin = ui.max_rect();
        self.water.begin(Domain::basin(basin));
        self.water.set_floor(Some(Floor::shallow(basin)));
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
                        legend(ui, "DONE", WHITE);
                        legend(ui, "GOAL", VIOLET);
                        legend(ui, "WORKING", GREEN);
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
                                    chrome::muted("NO MANUAL CODEX TERMINALS FOUND").size(13.0),
                                )
                            });
                        } else if let Some(census) = &self.census {
                            gallery(
                                ui,
                                &census.cards,
                                &mut self.water,
                                &self.nexus,
                                &mut self.hovered,
                            );
                        }
                    });
                scroll.state.offset.y
            });
        self.water.heave(ui.ctx(), panel.inner);
    }

    fn close_requested(&mut self) -> CloseDisposition {
        if self.tray.as_ref().is_some_and(Tray::available) {
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
        let destination = self.summon.swap(NO_DESKTOP, Ordering::AcqRel);
        let summoned = if destination == NO_DESKTOP {
            false
        } else {
            if let Err(error) =
                crate::desktop::Desktop::summon_process_to(std::process::id(), destination)
            {
                eprintln!("codex-wrangler could not summon itself: {error:#}");
            }
            true
        };
        first || summoned
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
    cards: &[CodexCard],
    water: &mut Surface,
    nexus: &Nexus,
    hovered: &mut Option<usize>,
) {
    let width = ui.available_width();
    let mut columns = 1_usize;
    let mut column_count = 1.0_f32;
    while (column_count + 1.0) * TILE_MIN + column_count * GAP <= width {
        columns += 1;
        column_count += 1.0;
    }
    let tile_width = ((width - GAP * (column_count - 1.0)) / column_count).max(180.0);
    for (row_index, row) in cards.chunks(columns).enumerate() {
        let _row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (column, codex) in row.iter().enumerate() {
                card(
                    ui,
                    codex,
                    row_index * columns + column,
                    tile_width,
                    water,
                    nexus,
                    hovered,
                );
            }
        });
        ui.add_space(GAP);
    }
}

fn card(
    ui: &mut egui::Ui,
    card: &CodexCard,
    index: usize,
    width: f32,
    water: &mut Surface,
    nexus: &Nexus,
    hovered_card: &mut Option<usize>,
) {
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
    let badge_width = card
        .workspace
        .map_or(0.0, |workspace| paint_workspace_badge(ui, rect, workspace));

    let inner = rect.shrink2(Vec2::new(14.0, 11.0));
    let mut body = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("codex-card-body", &card.thread))
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
    ui.painter().circle_filled(
        rect.right_bottom() - egui::vec2(13.0, 13.0),
        4.5,
        work_color(card.work),
    );

    // This interaction is deliberately registered after every inert child.
    // One final authority owns the entire rectangle, including text pixels.
    let response = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    dwemer_poolrooms::poolroom_anchor!(ui, CardTarget(&card.thread).to_string(), response.rect);
    if response.hovered() {
        *hovered_card = Some(index);
        water.hover(("codex", &card.thread), rect);
    }
    if response.clicked() {
        water.click(rect);
        let _sent = nexus.strike.try_send(Strike::Activate(card.window));
    }
}

fn legend(ui: &mut egui::Ui, label: &str, color: Color32) {
    let _legend = ui.horizontal(|ui| {
        let (rect, _response) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.25, color);
        let _label = ui.label(RichText::new(label).small().color(chrome::MUTED));
    });
}

fn paint_workspace_badge(ui: &egui::Ui, tile: egui::Rect, workspace: u32) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        workspace.to_string(),
        egui::FontId::new(14.0, egui::FontFamily::Monospace),
        chrome::TEXT,
    );
    let size = galley.size() + egui::vec2(12.0, 6.0);
    let rect = egui::Rect::from_min_size(egui::pos2(tile.right() - size.x, tile.top()), size);
    let radius = egui::CornerRadius {
        nw: 0,
        ne: 2,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(rect, radius, chrome::RAISED);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, chrome::EDGE_STRONG),
        StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, chrome::TEXT);
    size.x
}

const fn work_color(work: Work) -> Color32 {
    match work {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_palette_is_exact() {
        assert_eq!(work_color(Work::Turn), GREEN);
        assert_eq!(work_color(Work::Goal), VIOLET);
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
