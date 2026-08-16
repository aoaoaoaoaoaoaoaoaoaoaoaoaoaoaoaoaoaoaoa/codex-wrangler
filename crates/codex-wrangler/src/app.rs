use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(feature = "egui-test")]
use crate::contract::{
    CardKey, CardObservation, CardTarget, ClosePreference, DeleteGuard, Flight, GuideVisibility,
    HistoryObservation, HistorySortObservation, HistorySortTarget, HistoryTarget, Observation,
    PreferenceTarget, SearchObservation, SearchTarget, Tab, TabTarget, UI_FINGERPRINT,
    WorkspaceTarget,
};
use brass_poolrooms::{
    chrome,
    chrome::MechanismSize,
    water::{Domain, Floor, Frame as WaterFrame, Poke, Surface, Wetness},
};
use egui::{
    Color32, RichText, Sense, Stroke, StrokeKind, Vec2,
    text::{LayoutJob, TextFormat},
};
use eternalist_apps::{
    CloseDisposition, LivingWait, NativeApp, NativeWake, WindowSpec,
    command_guide::CommandGuide,
    commands::{CommandDispatch, CommandStatus},
};

use crate::{
    commands::{Edict, Realm, SCRY_IDIOMS, canon},
    contract::{Harness, HistoryColumn, SortDirection, Work},
    history::{
        Census as HistoryCensus, Nexus as HistoryNexus, Operation as HistoryOperation,
        Order as HistoryOrder, Session as HistorySession, spawn as spawn_history,
    },
    instance::{Incumbent, NO_DESKTOP},
    model::{Card, Census},
    posture::{Ledger, Posture},
    preferences::Preferences,
    recon::{Intent, Nexus, Strike, spawn},
    scry::{HistoryHit, HistoryScry, Hit, Scry},
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
const HISTORY_ROW: f32 = 34.0;

type JoltLedger = HashMap<Harness, HashMap<String, Vec2>>;

struct CardPhysics<'a> {
    jiggling: bool,
    recoiling: bool,
    water: &'a mut Surface,
    jolts: &'a mut JoltLedger,
    hovered: &'a mut Option<usize>,
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum SearchFocus {
    Idle,
    Seeking,
    Held,
    Releasing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Live,
    Historical,
}

impl Page {
    const ALL: [Self; 2] = [Self::Live, Self::Historical];

    const fn label(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::Historical => "HISTORICAL",
        }
    }
}

struct HistoryGesture {
    thread: String,
    operation: HistoryOperation,
}

impl SearchFocus {
    const fn held(self) -> bool {
        matches!(self, Self::Held)
    }

    const fn editing(self) -> bool {
        matches!(self, Self::Seeking | Self::Held)
    }
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
    historian: HistoryNexus,
    census: Option<Census>,
    history: Option<HistoryCensus>,
    census_label: String,
    first_frame_presented: bool,
    summon: Arc<AtomicU64>,
    summon_generation: u32,
    summon_attempts: Option<u8>,
    posture: Posture,
    pending_activation: Option<ActivationFlight>,
    quit: Arc<AtomicBool>,
    hovered: Option<usize>,
    history_hovered: Option<String>,
    search_focus: SearchFocus,
    jiggling: bool,
    jolts: JoltLedger,
    scry: Scry,
    history_scry: HistoryScry,
    history_requested: HashSet<(String, i64)>,
    history_pending: HashMap<String, HistoryOperation>,
    history_error: Option<String>,
    delete_target: Option<String>,
    page: Page,
    preferences: Preferences,
    guide: CommandGuide,
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
        let historian = spawn_history(wake.clone());
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
            historian,
            census: None,
            history: None,
            census_label: "DISCOVERING MANUAL THREADS".to_owned(),
            first_frame_presented: false,
            summon,
            summon_generation: 0,
            summon_attempts: Some(0),
            posture: Posture::from_floating(START_FLOATING),
            pending_activation: None,
            quit,
            hovered: None,
            history_hovered: None,
            search_focus: SearchFocus::Idle,
            jiggling: false,
            jolts: JoltLedger::new(),
            scry: Scry::default(),
            history_scry: HistoryScry::default(),
            history_requested: HashSet::new(),
            history_pending: HashMap::new(),
            history_error: None,
            delete_target: None,
            page: Page::Live,
            preferences: Preferences::restore(),
            guide: CommandGuide::default(),
            visibility: GalleryVisibility::Visible,
            ledger,
            tray,
        }
    }

    fn quench(&mut self) {
        self.clear_search();
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
        self.scry.reconcile(&census.cards);
        self.census_label = self.scry.label().to_owned();
        self.census = Some(census);
        self.reconcile_history_scry();
        true
    }

    fn drain_history(&mut self) -> bool {
        if let Some(census) = self.historian.take_census() {
            self.history = Some(census);
            self.reconcile_history_scry();
            return true;
        }
        false
    }

    fn reap_history_outcomes(&mut self) -> bool {
        let mut changed = false;
        for outcome in self.historian.take_outcomes() {
            let _prior = self.history_pending.remove(&outcome.order.thread);
            self.history_error = outcome.error.map(|error| {
                format!(
                    "{} {} FAILED · {error}",
                    outcome.order.operation.present_participle(),
                    outcome.order.thread
                )
            });
            changed = true;
        }
        changed
    }

    fn reconcile_history_scry(&mut self) {
        let Some(live) = live_codex_threads(self.census.as_ref()) else {
            self.history_scry.reconcile(&[], &HashSet::new());
            return;
        };
        let sessions = self
            .history
            .as_ref()
            .map_or(&[][..], |census| census.sessions.as_slice());
        self.history_scry.reconcile(sessions, &live);
    }

    fn clear_search(&mut self) {
        match self.page {
            Page::Live => {
                let cards = self
                    .census
                    .as_ref()
                    .map_or(&[][..], |census| census.cards.as_slice());
                self.scry.clear(cards);
                if self.census.is_some() {
                    self.census_label = self.scry.label().to_owned();
                }
            }
            Page::Historical => {
                if let Some(live) = live_codex_threads(self.census.as_ref()) {
                    let sessions = self
                        .history
                        .as_ref()
                        .map_or(&[][..], |census| census.sessions.as_slice());
                    self.history_scry.clear(sessions, &live);
                } else {
                    self.history_scry.clear(&[], &HashSet::new());
                }
            }
        }
        self.search_focus = SearchFocus::Releasing;
    }

    fn search_field(&mut self, ui: &mut egui::Ui, id: egui::Id) {
        match self.page {
            Page::Live => self.live_search_field(ui, id),
            Page::Historical => self.history_search_field(ui, id),
        }
    }

    fn live_search_field(&mut self, ui: &mut egui::Ui, id: egui::Id) {
        let before = self.scry.query().to_owned();
        let color = if self.scry.valid() { chrome::TEXT } else { RED };
        let response = ui.add(
            egui::TextEdit::singleline(self.scry.edit())
                .id(id)
                .hint_text("CASE-INSENSITIVE REGEXP · TITLES OR NAMELESS PATHS")
                .text_color(color)
                .desired_width(ui.available_width()),
        );
        brass_poolrooms::poolroom_anchor!(ui, SearchTarget::Editor.to_string(), response.rect);
        if self.search_focus == SearchFocus::Seeking {
            response.request_focus();
        }
        self.search_focus = if response.has_focus() {
            SearchFocus::Held
        } else {
            SearchFocus::Idle
        };
        if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            ui.ctx().request_discard("Search editor submitted");
        }
        if response.changed() {
            let cards = self
                .census
                .as_ref()
                .map_or(&[][..], |census| census.cards.as_slice());
            self.scry.revise(cards);
            if self.census.is_some() {
                self.census_label = self.scry.label().to_owned();
            }
        }
        if let Some(wake) = chrome::text_wake(ui, &response, &before, self.scry.query()) {
            self.water.text(wake);
        }
    }

    fn history_search_field(&mut self, ui: &mut egui::Ui, id: egui::Id) {
        let before = self.history_scry.query().to_owned();
        let color = if self.history_scry.valid() {
            chrome::TEXT
        } else {
            RED
        };
        let response = ui.add(
            egui::TextEdit::singleline(self.history_scry.edit())
                .id(id)
                .hint_text("CASE-INSENSITIVE REGEXP · SESSION NAMES OR IDS")
                .text_color(color)
                .desired_width(ui.available_width()),
        );
        brass_poolrooms::poolroom_anchor!(ui, SearchTarget::Editor.to_string(), response.rect);
        if self.search_focus == SearchFocus::Seeking {
            response.request_focus();
        }
        self.search_focus = if response.has_focus() {
            SearchFocus::Held
        } else {
            SearchFocus::Idle
        };
        if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            ui.ctx().request_discard("Search editor submitted");
        }
        if response.changed() {
            if let Some(live) = live_codex_threads(self.census.as_ref()) {
                let sessions = self
                    .history
                    .as_ref()
                    .map_or(&[][..], |census| census.sessions.as_slice());
                self.history_scry.revise(sessions, &live);
            } else {
                self.history_scry.revise(&[], &HashSet::new());
            }
        }
        if let Some(wake) = chrome::text_wake(ui, &response, &before, self.history_scry.query()) {
            self.water.text(wake);
        }
    }

    fn close_preference(&mut self, ui: &mut egui::Ui) {
        let mut minimize = self.preferences.minimize_on_close();
        let latch = chrome::Checkbox::new(&mut minimize, "MINIMIZE ON CLOSE")
            .label_side(chrome::LabelSide::Left)
            .size(MechanismSize::Small)
            .show(ui);
        brass_poolrooms::poolroom_anchor!(
            ui,
            PreferenceTarget("minimize-on-close").to_string(),
            latch.rect
        );
        self.water.checkbox(&latch);
        if latch.changed() {
            self.preferences.set_minimize_on_close(minimize);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let _heading = ui.horizontal(|ui| {
            let _title = ui.label(chrome::title("CODEX WRANGLER").size(18.0));
            ui.add_space(8.0);
            let (label, valid) = match self.page {
                Page::Live => (self.census_label.as_str(), self.scry.valid()),
                Page::Historical => (self.history_scry.label(), self.history_scry.valid()),
            };
            let count = if valid {
                chrome::muted(label).size(13.0)
            } else {
                RichText::new(label).size(13.0).color(RED)
            };
            let _count = ui.label(count);
            ui.add_space(8.0);
            let _help = ui.label(chrome::muted("? TO OPEN HELP").size(12.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.close_preference(ui);
                ui.add_space(8.0);
                if self.page == Page::Live {
                    legend(ui, "DONE", Work::Done);
                    legend(ui, "SLEEP", Work::Sleep);
                    legend(ui, "CLOSED", Work::Closed);
                    legend(ui, "GOAL", Work::Goal);
                    legend(ui, "WORKING", Work::Turn);
                    legend(ui, "INPUT", Work::Input);
                    legend(ui, "ERROR", Work::Error);
                } else {
                    let mut guarded = self.preferences.confirm_deletion();
                    let guard = chrome::Checkbox::new(&mut guarded, "CONFIRM DELETE")
                        .label_side(chrome::LabelSide::Left)
                        .size(MechanismSize::Small)
                        .show(ui);
                    brass_poolrooms::poolroom_anchor!(
                        ui,
                        HistoryTarget("preferences", "confirm-delete").to_string(),
                        guard.rect
                    );
                    self.water.checkbox(&guard);
                    if guard.changed() {
                        self.preferences.set_confirm_deletion(guarded);
                    }
                }
            });
        });
        ui.add_space(7.0);
        let _tabs = ui.horizontal(|ui| {
            for page in Page::ALL {
                let response = page_tab(ui, page, self.page == page);
                brass_poolrooms::poolroom_anchor!(
                    ui,
                    TabTarget(page.into()).to_string(),
                    response.rect
                );
                if response.hovered() {
                    self.water
                        .hover(("wrangler-tab", page.label()), response.rect);
                }
                if response.clicked() && self.page != page {
                    self.water.click(response.rect);
                    self.page = page;
                    self.search_focus = SearchFocus::Releasing;
                    ui.ctx().request_discard("Wrangler tab changed");
                }
            }
            let (query, valid) = match self.page {
                Page::Live => (self.scry.query(), self.scry.valid()),
                Page::Historical => (self.history_scry.query(), self.history_scry.valid()),
            };
            if !self.search_focus.editing() && !query.is_empty() {
                ui.add_space(10.0);
                let filter = format!("FILTER · {query}");
                let text = if valid {
                    chrome::muted(filter).size(12.0)
                } else {
                    RichText::new(filter).size(12.0).color(RED)
                };
                let filter_rect = ui
                    .add(
                        egui::Label::new(text)
                            .truncate()
                            .show_tooltip_when_elided(false),
                    )
                    .rect;
                brass_poolrooms::poolroom_anchor!(
                    ui,
                    SearchTarget::Filter.to_string(),
                    filter_rect
                );
                #[cfg(not(feature = "egui-test"))]
                let _ = filter_rect;
            }
        });
    }

    fn gallery_panel(
        &mut self,
        ui: &mut egui::Ui,
        search_id: egui::Id,
        jiggling: bool,
        recoiling: bool,
    ) -> (Option<Strike>, f32) {
        let mut selected = None;
        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(12, 11, 9, 232))
                    .inner_margin(22),
            )
            .show(ui, |ui| {
                self.header(ui);
                if self.search_focus.editing() {
                    ui.add_space(7.0);
                    self.search_field(ui, search_id);
                }
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
                        } else if self.scry.hits().is_empty() {
                            let _empty = ui.centered_and_justified(|ui| {
                                ui.label(chrome::muted("NO MATCHING SESSIONS").size(13.0))
                            });
                        } else if let Some(census) = &self.census {
                            selected = gallery(
                                ui,
                                &census.cards,
                                self.scry.hits(),
                                &mut CardPhysics {
                                    jiggling,
                                    recoiling,
                                    water: &mut self.water,
                                    jolts: &mut self.jolts,
                                    hovered: &mut self.hovered,
                                },
                            );
                        }
                    });
                scroll.state.offset.y
            });
        (selected, panel.inner)
    }

    fn history_panel(
        &mut self,
        ui: &mut egui::Ui,
        search_id: egui::Id,
    ) -> (Option<HistoryGesture>, f32) {
        let mut gesture = None;
        let mut inspect = Vec::new();
        let panel = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(12, 11, 9, 232))
                    .inner_margin(22),
            )
            .show(ui, |ui| {
                self.header(ui);
                if self.search_focus.editing() {
                    ui.add_space(7.0);
                    self.search_field(ui, search_id);
                }
                if let Some(error) = &self.history_error {
                    ui.add_space(7.0);
                    let _error = ui.add(
                        egui::Label::new(RichText::new(error).size(12.0).color(RED))
                            .wrap()
                            .show_tooltip_when_elided(false),
                    );
                }
                ui.add_space(14.0);
                if self.history_barrier(ui) {
                    return 0.0;
                }
                if self.history_scry.hits().is_empty() {
                    let message = if self.history_scry.query().is_empty() {
                        "NO HISTORICAL SESSIONS"
                    } else {
                        "NO MATCHING HISTORICAL SESSIONS"
                    };
                    let _empty =
                        ui.centered_and_justified(|ui| ui.label(chrome::muted(message).size(13.0)));
                    return 0.0;
                }
                let width = ui.available_width();
                let columns = HistoryColumns::fit(width);
                if let Some(column) =
                    history_header(ui, columns, &self.history_scry, &mut self.water)
                {
                    let sessions = &self.history.as_ref().expect("history exists").sessions;
                    self.history_scry.cycle(column, sessions);
                    ui.ctx().request_discard("Historical order changed");
                }
                ui.add_space(4.0);
                let scroll = egui::ScrollArea::vertical()
                    .id_salt("codex-history")
                    .auto_shrink([false; 2])
                    .show_rows(
                        ui,
                        HISTORY_ROW,
                        self.history_scry.hits().len(),
                        |ui, rows| {
                            let sessions = &self.history.as_ref().expect("history exists").sessions;
                            let result = history_rows(
                                ui,
                                sessions,
                                self.history_scry.hits(),
                                rows,
                                columns,
                                &self.history_pending,
                                &mut self.water,
                                &mut self.history_hovered,
                            );
                            gesture = result.gesture;
                            inspect = result.inspect;
                        },
                    );
                scroll.state.offset.y
            });
        let stamps = self
            .history
            .iter()
            .flat_map(|history| &history.sessions)
            .map(|session| (session.thread.as_str(), session.updated_at_ms))
            .collect::<HashMap<_, _>>();
        let novel = inspect
            .into_iter()
            .filter(|thread| {
                stamps.get(thread.as_str()).is_some_and(|updated_at_ms| {
                    self.history_requested
                        .insert((thread.clone(), *updated_at_ms))
                })
            })
            .collect::<Vec<_>>();
        if !novel.is_empty() && self.historian.courier().inspect(novel.clone()).is_err() {
            for thread in novel {
                if let Some(updated_at_ms) = stamps.get(thread.as_str()) {
                    let _removed = self.history_requested.remove(&(thread, *updated_at_ms));
                }
            }
        }
        (gesture, panel.inner)
    }

    fn history_barrier(&mut self, ui: &mut egui::Ui) -> bool {
        if self.history.is_none() || self.census.is_none() {
            let arena = ui.max_rect();
            let _bouncer = self.living_wait.bouncer(ui, arena);
            return true;
        }
        if let Some(fault) = self
            .census
            .as_ref()
            .and_then(|census| census.fault.as_deref())
        {
            let _fault = ui.label(
                RichText::new(format!("COULD NOT PARTITION LIVE SESSIONS · {fault}"))
                    .color(Color32::LIGHT_RED),
            );
            return true;
        }
        if let Some(fault) = self
            .history
            .as_ref()
            .and_then(|history| history.fault.as_deref())
        {
            let _fault = ui.label(RichText::new(fault).color(Color32::LIGHT_RED));
            return true;
        }
        false
    }

    fn submit_history(&mut self, order: HistoryOrder) {
        if self.history_pending.contains_key(&order.thread) {
            return;
        }
        if self.historian.courier().order(order.clone()).is_ok() {
            self.history_error = None;
            let _prior = self.history_pending.insert(order.thread, order.operation);
        }
    }

    fn accept_history_gesture(&mut self, gesture: HistoryGesture) {
        if gesture.operation == HistoryOperation::Delete && self.preferences.confirm_deletion() {
            self.delete_target = Some(gesture.thread);
        } else {
            self.submit_history(HistoryOrder {
                thread: gesture.thread,
                operation: gesture.operation,
            });
        }
    }

    fn deletion_modal(&mut self, ctx: &egui::Context) {
        let Some(thread) = self.delete_target.clone() else {
            return;
        };
        let name = self
            .history
            .as_ref()
            .and_then(|history| {
                history
                    .sessions
                    .iter()
                    .find(|session| session.thread == thread)
            })
            .and_then(|session| session.name.as_deref())
            .unwrap_or("anonymous")
            .to_owned();
        let enter =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let mut cancel = false;
        let mut delete = enter;
        let modal = egui::Modal::new(egui::Id::new("codex-history-delete"))
            .frame(
                egui::Frame::new()
                    .fill(chrome::SURFACE)
                    .stroke(Stroke::new(1.5, RED))
                    .corner_radius(2)
                    .inner_margin(egui::Margin::same(18)),
            )
            .backdrop_color(Color32::from_black_alpha(188))
            .show(ctx, |ui| {
                ui.set_width(520.0_f32.min(ctx.content_rect().width() - 48.0));
                let _title = ui.label(chrome::title("DELETE SESSION PERMANENTLY?"));
                ui.add_space(9.0);
                let _name = ui.label(RichText::new(name).color(chrome::TEXT));
                let _thread = ui.label(RichText::new(&thread).size(11.0).color(chrome::MUTED));
                ui.add_space(10.0);
                let _warning = ui.label(
                    RichText::new("The rollout and its Codex index record will be destroyed.")
                        .color(RED),
                );
                ui.add_space(10.0);
                let mut guarded = self.preferences.confirm_deletion();
                let guard = chrome::Checkbox::new(&mut guarded, "CONFIRM FUTURE DELETIONS")
                    .size(MechanismSize::Small)
                    .show(ui);
                brass_poolrooms::poolroom_anchor!(
                    ui,
                    HistoryTarget(&thread, "confirm-future").to_string(),
                    guard.rect
                );
                self.water.checkbox(&guard);
                if guard.changed() {
                    self.preferences.set_confirm_deletion(guarded);
                }
                ui.add_space(12.0);
                let _buttons = ui.horizontal(|ui| {
                    let cancel_button =
                        ui.add(egui::Button::new("CANCEL").min_size(Vec2::new(100.0, 30.0)));
                    chrome::shallow_tension(ui, &cancel_button);
                    cancel |= cancel_button.clicked();
                    let delete_button = ui.add(
                        egui::Button::new(RichText::new("DELETE").strong().color(RED))
                            .min_size(Vec2::new(120.0, 30.0))
                            .stroke(Stroke::new(1.4, RED)),
                    );
                    chrome::shallow_tension(ui, &delete_button);
                    brass_poolrooms::poolroom_anchor!(
                        ui,
                        HistoryTarget(&thread, "confirm-delete").to_string(),
                        delete_button.rect
                    );
                    delete |= delete_button.clicked();
                });
            });
        if delete {
            self.delete_target = None;
            self.submit_history(HistoryOrder {
                thread,
                operation: HistoryOperation::Delete,
            });
        } else if cancel || modal.should_close() {
            self.delete_target = None;
        }
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
    fn search_observation(&self) -> SearchObservation {
        let (query, valid) = match self.page {
            Page::Live => (self.scry.query(), self.scry.valid()),
            Page::Historical => (self.history_scry.query(), self.history_scry.valid()),
        };
        SearchObservation {
            query: query.to_owned(),
            valid,
            focused: self.search_focus.held(),
            editing: self.search_focus.editing(),
        }
    }

    #[cfg(feature = "egui-test")]
    fn observation(&self) -> Observation {
        let live = live_codex_threads(self.census.as_ref());
        let history = live.as_ref().map_or_else(Vec::new, |live| {
            self.history
                .iter()
                .flat_map(|history| &history.sessions)
                .filter(|session| !live.contains(&session.thread))
                .map(|session| HistoryObservation {
                    thread: session.thread.clone(),
                    name: session.name.clone(),
                    turns: session.turns,
                    bytes: session.bytes,
                    archived: session.archived,
                })
                .collect()
        });
        let history_order = self.history.as_ref().map_or_else(Vec::new, |history| {
            self.history_scry
                .hits()
                .iter()
                .filter_map(|hit| history.sessions.get(hit.session()))
                .map(|session| session.thread.clone())
                .collect()
        });
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
            search: self.search_observation(),
            guide: if self.guide.is_open() {
                GuideVisibility::Open
            } else {
                GuideVisibility::Closed
            },
            tab: self.page.into(),
            delete_guard: if self.preferences.confirm_deletion() {
                DeleteGuard::Armed
            } else {
                DeleteGuard::Bypassed
            },
            close_preference: if self.preferences.minimize_on_close() {
                ClosePreference::Minimize
            } else {
                ClosePreference::Exit
            },
            delete_prompt: self.delete_target.clone(),
            visible: self
                .census
                .iter()
                .flat_map(|census| {
                    self.scry
                        .hits()
                        .iter()
                        .filter_map(|hit| census.cards.get(hit.card()))
                })
                .map(|card| CardKey {
                    harness: card.harness,
                    thread: card.thread.clone(),
                })
                .collect(),
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
            history,
            history_order,
            history_sorts: self
                .history_scry
                .sorts()
                .map(|(column, direction)| HistorySortObservation { column, direction })
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
        self.history_hovered = None;
        self.reap_activation();
        let history_outcome = self.reap_history_outcomes();
        if self.visibility != GalleryVisibility::Visible {
            return;
        }
        let search_id = ui.make_persistent_id("thread-search");
        if self.search_focus == SearchFocus::Releasing {
            ui.memory_mut(|memory| memory.surrender_focus(search_id));
            self.search_focus = SearchFocus::Idle;
        }
        let modal_open = self.delete_target.is_some();
        let help_invoked = !modal_open && self.guide.take_shortcuts(ui.ctx());
        if !modal_open
            && !help_invoked
            && !self.guide.is_open()
            && let Some(CommandDispatch::Invoke(Edict::Scry)) =
                canon().route(ui.ctx(), &[Realm::Gallery], |_| CommandStatus::Enabled)
        {
            self.search_focus = SearchFocus::Seeking;
        }
        if !modal_open
            && !self.guide.is_open()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.clear_search();
            ui.memory_mut(|memory| memory.surrender_focus(search_id));
        }
        let basin = ui.max_rect();
        let jiggling = !modal_open
            && self.page == Page::Live
            && !self.guide.is_open()
            && !ui.ctx().text_edit_focused()
            && ui.input(|input| input.modifiers.shift);
        let recoiling = self.jiggling && !jiggling;
        self.jiggling = jiggling;
        if jiggling {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(45));
        }
        self.water.begin(Domain::basin(basin));
        self.water.set_floor(Some(Floor::shallow(basin)));
        let (selected, historical, heave) = match self.page {
            Page::Live => {
                let (selected, heave) = self.gallery_panel(ui, search_id, jiggling, recoiling);
                (selected, None, heave)
            }
            Page::Historical => {
                let (gesture, heave) = self.history_panel(ui, search_id);
                (None, gesture, heave)
            }
        };
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
        if let Some(gesture) = historical {
            self.accept_history_gesture(gesture);
        }
        self.deletion_modal(ui.ctx());
        self.water.heave(ui.ctx(), heave);
        self.guide.show(
            ui.ctx(),
            canon(),
            &[Realm::Gallery],
            |_| match self.page {
                Page::Live => "LIVE",
                Page::Historical => "HISTORICAL",
            },
            |_| CommandStatus::Enabled,
            &[SCRY_IDIOMS],
        );
        let stable_pointer = match self.page {
            Page::Live => self.hovered.is_none(),
            Page::Historical => self.history_hovered.is_none(),
        };
        let mut changed = history_outcome;
        if stable_pointer && !jiggling && !self.search_focus.held() {
            changed |= self.drain();
            changed |= self.drain_history();
        }
        if changed {
            ui.ctx().request_repaint();
        }
    }

    fn close_requested(&mut self) -> CloseDisposition {
        if self.preferences.minimize_on_close() && self.tray.as_ref().is_some_and(Tray::available) {
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

#[cfg(feature = "egui-test")]
impl From<Page> for Tab {
    fn from(page: Page) -> Self {
        match page {
            Page::Live => Self::Live,
            Page::Historical => Self::Historical,
        }
    }
}

fn live_codex_threads(census: Option<&Census>) -> Option<HashSet<String>> {
    let census = census.filter(|census| census.fault.is_none())?;
    Some(
        census
            .cards
            .iter()
            .filter(|card| card.harness == Harness::Codex)
            .map(|card| card.thread.clone())
            .collect(),
    )
}

fn page_tab(ui: &mut egui::Ui, page: Page, selected: bool) -> egui::Response {
    let button = egui::Button::new(chrome::section_title(page.label()))
        .min_size(Vec2::new(142.0, 28.0))
        .fill(if selected {
            chrome::RAISED
        } else {
            chrome::CONTROL
        })
        .stroke(Stroke::new(
            if selected { 1.5 } else { 1.0 },
            if selected { chrome::HOT } else { chrome::EDGE },
        ));
    let response = ui.add(button);
    chrome::shallow_tension(ui, &response);
    response
}

#[derive(Clone, Copy)]
struct HistoryColumns {
    id: f32,
    name: f32,
    date: f32,
    turns: f32,
    size: f32,
    state: f32,
    action: f32,
    delete: f32,
}

impl HistoryColumns {
    fn fit(width: f32) -> Self {
        const FIXED: f32 = 270.0 + 136.0 + 58.0 + 82.0 + 82.0 + 104.0 + 42.0;
        Self {
            id: 270.0,
            name: (width - FIXED).max(120.0),
            date: 136.0,
            turns: 58.0,
            size: 82.0,
            state: 82.0,
            action: 104.0,
            delete: 42.0,
        }
    }
}

struct HistoryRows {
    gesture: Option<HistoryGesture>,
    inspect: Vec<String>,
}

fn history_header(
    ui: &mut egui::Ui,
    columns: HistoryColumns,
    scry: &HistoryScry,
    water: &mut Surface,
) -> Option<HistoryColumn> {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HISTORY_ROW), Sense::hover());
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("history-header")
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 0.0;
    let mut selected = None;
    for (width, label, column) in [
        (columns.id, "SESSION ID", HistoryColumn::SessionId),
        (columns.name, "NAME", HistoryColumn::Name),
        (columns.date, "LAST TURN", HistoryColumn::LastTurn),
        (columns.turns, "TURNS", HistoryColumn::Turns),
        (columns.size, "SIZE", HistoryColumn::Size),
        (columns.state, "STATE", HistoryColumn::State),
    ] {
        if history_sort_cell(
            &mut row,
            width,
            label,
            column,
            scry.direction(column),
            water,
        ) {
            selected = Some(column);
        }
    }
    for width in [columns.action, columns.delete] {
        history_cell(&mut row, width, |_| {});
    }
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, chrome::EDGE_STRONG),
    );
    selected
}

fn history_sort_cell(
    row: &mut egui::Ui,
    width: f32,
    label: &'static str,
    column: HistoryColumn,
    direction: Option<SortDirection>,
    water: &mut Surface,
) -> bool {
    let mut clicked = false;
    history_cell(row, width, |ui| {
        let label = match direction {
            Some(SortDirection::Ascending) => format!("{label} ↑"),
            Some(SortDirection::Descending) => format!("{label} ↓"),
            None => label.to_owned(),
        };
        let mut text = chrome::eyebrow(label).size(11.0);
        if direction.is_some() {
            text = text.color(chrome::HOT);
        }
        let response = ui.add_sized(
            [ui.available_width(), ui.available_height()],
            egui::Label::new(text).sense(Sense::click()),
        );
        chrome::shallow_tension(ui, &response);
        brass_poolrooms::poolroom_anchor!(ui, HistorySortTarget(column).to_string(), response.rect);
        if response.hovered() {
            water.hover(("history-sort", column), response.rect);
        }
        clicked = response.clicked();
        if clicked {
            water.click(response.rect);
        }
    });
    clicked
}

#[allow(clippy::too_many_arguments)]
fn history_rows(
    ui: &mut egui::Ui,
    sessions: &[HistorySession],
    hits: &[HistoryHit],
    rows: std::ops::Range<usize>,
    columns: HistoryColumns,
    pending: &HashMap<String, HistoryOperation>,
    water: &mut Surface,
    hovered: &mut Option<String>,
) -> HistoryRows {
    let mut gesture = None;
    let mut inspect = Vec::new();
    for visible in rows {
        let hit = &hits[visible];
        let session = &sessions[hit.session()];
        if session.turns.is_none() && !session.tally_failed {
            inspect.push(session.thread.clone());
        }
        let found = ui
            .push_id(&session.thread, |ui| {
                history_row(
                    ui,
                    visible,
                    session,
                    hit,
                    columns,
                    pending.get(&session.thread).copied(),
                    water,
                    hovered,
                )
            })
            .inner;
        if gesture.is_none() {
            gesture = found;
        }
    }
    HistoryRows { gesture, inspect }
}

#[allow(clippy::too_many_arguments)]
fn history_row(
    ui: &mut egui::Ui,
    visible: usize,
    session: &HistorySession,
    hit: &HistoryHit,
    columns: HistoryColumns,
    flight: Option<HistoryOperation>,
    water: &mut Surface,
    hovered: &mut Option<String>,
) -> Option<HistoryGesture> {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), HISTORY_ROW), Sense::hover());
    let pointer_inside = ui.rect_contains_pointer(rect);
    let fill = if pointer_inside {
        Color32::from_rgba_unmultiplied(36, 30, 22, 210)
    } else if visible.is_multiple_of(2) {
        Color32::from_rgba_unmultiplied(18, 16, 13, 190)
    } else {
        Color32::from_rgba_unmultiplied(13, 12, 10, 176)
    };
    ui.painter().rect_filled(rect, 1, fill);
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("history-row")
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 0.0;
    history_facts(&mut row, session, hit, columns);
    let gesture = history_operation(&mut row, session, columns.action, flight, water)
        .or_else(|| history_delete(&mut row, session, columns.delete, flight, water));
    if pointer_inside {
        *hovered = Some(session.thread.clone());
        water.hover(("historical", &session.thread), rect);
    }
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, chrome::EDGE),
    );
    gesture
}

fn history_facts(
    row: &mut egui::Ui,
    session: &HistorySession,
    hit: &HistoryHit,
    columns: HistoryColumns,
) {
    history_cell(row, columns.id, |ui| {
        let _id = history_marked_label(ui, &session.thread, hit.id_spans(), chrome::MUTED, 11.0);
    });
    history_cell(row, columns.name, |ui| {
        if let Some(name) = session.name.as_deref() {
            let _name = history_marked_label(ui, name, hit.name_spans(), chrome::TEXT, 13.0);
        } else {
            let _anonymous = ui.add(
                egui::Label::new(RichText::new("anonymous").size(11.0).color(chrome::MUTED))
                    .truncate()
                    .show_tooltip_when_elided(false),
            );
        }
    });
    history_cell(row, columns.date, |ui| {
        let _date = ui.label(
            RichText::new(&session.last_turn)
                .size(12.0)
                .color(chrome::MUTED),
        );
    });
    history_cell(row, columns.turns, |ui| {
        let tally = session.turns.map_or_else(
            || if session.tally_failed { "ERR" } else { "…" }.to_owned(),
            |turns| turns.to_string(),
        );
        let color = if session.tally_failed {
            RED
        } else {
            chrome::TEXT
        };
        let _turns = ui.label(RichText::new(tally).size(12.0).color(color));
    });
    history_cell(row, columns.size, |ui| {
        let _size = ui.label(
            RichText::new(format_size(session.bytes))
                .size(12.0)
                .color(chrome::TEXT),
        );
    });
    history_cell(row, columns.state, |ui| {
        let (label, color) = if session.archived {
            ("ARCHIVED", chrome::HOT)
        } else {
            ("OPEN", chrome::MUTED)
        };
        let _state = ui.label(RichText::new(label).size(11.0).color(color));
    });
}

fn history_operation(
    row: &mut egui::Ui,
    session: &HistorySession,
    width: f32,
    flight: Option<HistoryOperation>,
    water: &mut Surface,
) -> Option<HistoryGesture> {
    let operation = if session.archived {
        HistoryOperation::Unarchive
    } else {
        HistoryOperation::Archive
    };
    let mut clicked = false;
    history_cell(row, width, |ui| {
        let label = flight.map_or_else(
            || match operation {
                HistoryOperation::Archive => "ARCHIVE",
                HistoryOperation::Unarchive => "UNARCHIVE",
                HistoryOperation::Delete => unreachable!(),
            },
            HistoryOperation::present_participle,
        );
        let response = ui.add_enabled(
            flight.is_none(),
            egui::Button::new(RichText::new(label).size(11.0)).min_size(Vec2::new(92.0, 24.0)),
        );
        chrome::shallow_tension(ui, &response);
        brass_poolrooms::poolroom_anchor!(
            ui,
            HistoryTarget(
                &session.thread,
                if operation == HistoryOperation::Archive {
                    "archive"
                } else {
                    "unarchive"
                }
            )
            .to_string(),
            response.rect
        );
        clicked = response.clicked();
        if clicked {
            water.click(response.rect);
        }
    });
    clicked.then(|| HistoryGesture {
        thread: session.thread.clone(),
        operation,
    })
}

fn history_delete(
    row: &mut egui::Ui,
    session: &HistorySession,
    width: f32,
    flight: Option<HistoryOperation>,
    water: &mut Surface,
) -> Option<HistoryGesture> {
    let mut clicked = false;
    history_cell(row, width, |ui| {
        let response = ui.add_enabled(
            flight.is_none(),
            egui::Button::new(RichText::new("×").size(19.0).strong().color(RED))
                .min_size(Vec2::new(32.0, 26.0))
                .stroke(Stroke::new(1.2, RED)),
        );
        chrome::shallow_tension(ui, &response);
        brass_poolrooms::poolroom_anchor!(
            ui,
            HistoryTarget(&session.thread, "delete").to_string(),
            response.rect
        );
        clicked = response.clicked();
        if clicked {
            water.click(response.rect);
        }
    });
    clicked.then(|| HistoryGesture {
        thread: session.thread.clone(),
        operation: HistoryOperation::Delete,
    })
}

fn history_cell(ui: &mut egui::Ui, width: f32, contents: impl FnOnce(&mut egui::Ui)) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, HISTORY_ROW), Sense::hover());
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .id_salt((rect.min.x.to_bits(), rect.min.y.to_bits()))
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    cell.set_clip_rect(cell.clip_rect().intersect(rect));
    cell.add_space(5.0);
    contents(&mut cell);
}

fn history_marked_label(
    ui: &mut egui::Ui,
    text: &str,
    spans: &[std::ops::Range<usize>],
    color: Color32,
    size: f32,
) -> egui::Response {
    let label = if spans.is_empty() {
        egui::Label::new(RichText::new(text).size(size).color(color))
    } else {
        let plain = TextFormat {
            font_id: egui::FontId::new(size, egui::FontFamily::Proportional),
            color,
            ..TextFormat::default()
        };
        let marked = TextFormat {
            color: WHITE,
            background: Color32::from_rgba_unmultiplied(235, 197, 151, 76),
            ..plain.clone()
        };
        let mut job = LayoutJob::default();
        let mut cursor = 0;
        for span in spans {
            job.append(&text[cursor..span.start], 0.0, plain.clone());
            job.append(&text[span.clone()], 0.0, marked.clone());
            cursor = span.end;
        }
        job.append(&text[cursor..], 0.0, plain);
        egui::Label::new(job)
    };
    ui.add(label.truncate().show_tooltip_when_elided(false))
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    match bytes {
        GIB.. => format_unit(bytes, GIB, "GiB"),
        MIB.. => format_unit(bytes, MIB, "MiB"),
        KIB.. => format_unit(bytes, KIB, "KiB"),
        _ => format!("{bytes} B"),
    }
}

fn format_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let tenth = bytes % unit * 10 / unit;
    format!("{whole}.{tenth} {suffix}")
}

fn gallery(
    ui: &mut egui::Ui,
    cards: &[Card],
    hits: &[Hit],
    physics: &mut CardPhysics<'_>,
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
    for row in hits.chunks(columns) {
        let _row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for hit in row {
                let card_model = &cards[hit.card()];
                let clicked = card(ui, card_model, hit, tile_width, physics);
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
    hit: &Hit,
    width: f32,
    physics: &mut CardPhysics<'_>,
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
    paint_card_contents(ui, visual, card, hit.spans());

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
        *physics.hovered = Some(hit.card());
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

fn paint_card_contents(
    ui: &mut egui::Ui,
    visual: egui::Rect,
    card: &Card,
    match_spans: &[std::ops::Range<usize>],
) {
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
    let name = card.name.as_deref().filter(|name| !name.is_empty());
    body.set_max_width((inner.width() - workspace_width).max(80.0));
    let _name = if let Some(name) = name {
        marked_label(&mut body, name, match_spans, chrome::TEXT)
    } else {
        body.add(
            egui::Label::new(RichText::new("anonymous").small().color(chrome::MUTED))
                .truncate()
                .show_tooltip_when_elided(false),
        )
    };
    body.set_max_width(inner.width());
    body.add_space(6.0);
    let path_spans = if name.is_none() { match_spans } else { &[] };
    let _cwd = marked_label(&mut body, &card.cwd, path_spans, chrome::HOT);
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

fn marked_label(
    ui: &mut egui::Ui,
    text: &str,
    spans: &[std::ops::Range<usize>],
    color: Color32,
) -> egui::Response {
    let label = if spans.is_empty() {
        egui::Label::new(RichText::new(text).color(color))
    } else {
        egui::Label::new(marked_text(ui, text, spans, color))
    };
    ui.add(label.truncate().show_tooltip_when_elided(false))
}

fn marked_text(
    ui: &egui::Ui,
    text: &str,
    spans: &[std::ops::Range<usize>],
    color: Color32,
) -> LayoutJob {
    let plain = TextFormat {
        font_id: egui::TextStyle::Body.resolve(ui.style()),
        color,
        ..TextFormat::default()
    };
    let marked = TextFormat {
        color: WHITE,
        background: Color32::from_rgba_unmultiplied(235, 197, 151, 76),
        ..plain.clone()
    };
    let mut job = LayoutJob::default();
    let mut cursor = 0;
    for span in spans {
        job.append(&text[cursor..span.start], 0.0, plain.clone());
        job.append(&text[span.clone()], 0.0, marked.clone());
        cursor = span.end;
    }
    job.append(&text[cursor..], 0.0, plain);
    job
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
