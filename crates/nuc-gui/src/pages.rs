//! Page assembly: sidebar navigation, per-page widgets and the one-second
//! UI tick that pulls the newest snapshot from the sampler thread.
//!
//! Every page renders "Unavailable" markers verbatim from the model —
//! values are never guessed or back-filled with zeros.

use adw::glib;
use adw::glib::ControlFlow;
use adw::gtk;
use adw::prelude::*;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use nuc_core::{AlertEvent, MutexExt, Snapshot, TempUnit};

use crate::GuiState;
use crate::chart;

struct Ui {
    dash: DashRefs,
    sensors: SensorsRefs,
    power: PowerRefs,
    history: HistoryRefs,
}

pub fn build(app: &adw::Application, state: Arc<Mutex<GuiState>>, startup_warning: Option<String>) {
    let mut timers = Vec::new();
    let cfg = state.lock_or_recover().cfg.clone();
    app.style_manager()
        .set_color_scheme(match cfg.general.theme.as_str() {
            "dark" => adw::ColorScheme::ForceDark,
            "light" => adw::ColorScheme::ForceLight,
            _ => adw::ColorScheme::Default,
        });
    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("NUC Control")
        .default_width(1040)
        .default_height(700)
        .build();

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    let sidebar = gtk::StackSidebar::new();
    sidebar.set_stack(&stack);

    let dash = build_dashboard(&stack);
    let sensors = build_sensors(&stack);
    let power = build_power(&stack, &mut timers);
    let history = build_history(state.clone(), &stack);
    build_diagnostics(state.clone(), &stack, &mut timers);
    build_settings(state.clone(), &stack);
    if stack.child_by_name(&cfg.general.start_page).is_some() {
        stack.set_visible_child_name(&cfg.general.start_page);
    }

    if let Some(w) = startup_warning {
        dash.banner.set_label(&w);
        dash.banner.set_visible(true);
    }

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let sidebar_container = gtk::ScrolledWindow::builder()
        .child(&sidebar)
        .width_request(185)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    content.append(&sidebar_container);
    content.append(&stack);

    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "NUC Control",
        "hardware & power monitor (read-first)",
    )));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&content);
    win.set_content(Some(&root));
    win.present();

    let ui = Rc::new(RefCell::new(Ui {
        dash,
        sensors,
        power,
        history,
    }));
    let ui_tick = ui.clone();
    let app_tick = app.clone();
    let state_tick = state.clone();
    timers.push(glib::timeout_add_local(
        std::time::Duration::from_millis(1000),
        move || {
            if let Ok(mut st) = state_tick.lock() {
                let mut ui = ui_tick.borrow_mut();
                tick(&app_tick, &mut st, &mut ui);
            }
            ControlFlow::Continue
        },
    ));
    let timers = RefCell::new(timers);
    win.connect_close_request(move |_| {
        for timer in timers.borrow_mut().drain(..) {
            timer.remove();
        }
        if let Ok(mut st) = state.lock() {
            st.handle.shutdown();
            let _ = st.profile_stop.send(());
            if let Err(error) = st.controls.done.recv() {
                eprintln!("sampler shutdown: {error}");
            }
            st.history.shutdown();
        }
        glib::Propagation::Proceed
    });
}

// ----------------------------------------------------------- shared helpers

fn page_container(stack: &gtk::Stack, name: &str, title: &str) -> gtk::Box {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_start(18)
        .margin_end(18)
        .margin_bottom(18)
        .build();
    scroll.set_child(Some(&box_));
    stack.add_titled(&scroll, Some(name), title);
    box_
}

fn tile(title: &str) -> (gtk::Frame, gtk::Label) {
    let value = gtk::Label::builder()
        .label("waiting …")
        .wrap(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let frame = gtk::Frame::builder()
        .label(title)
        .child(&value)
        .css_classes(["card"])
        .build();
    (frame, value)
}

fn section_title(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .css_classes(["title-4"])
        .build()
}

fn note(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build()
}

fn fmt_temp(c: Option<f64>, unit: TempUnit) -> String {
    c.map(|v| unit.format(v))
        .unwrap_or_else(|| "Unavailable".into())
}

fn clear_box(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

// ----------------------------------------------------------- dashboard

struct DashRefs {
    banner: gtk::Label,
    tile_cpu: gtk::Label,
    tile_power: gtk::Label,
    tile_temp: gtk::Label,
    tile_gpu: gtk::Label,
    tile_ram: gtk::Label,
    tile_fan: gtk::Label,
    tile_profile: gtk::Label,
    tile_peak: gtk::Label,
}

fn build_dashboard(stack: &gtk::Stack) -> DashRefs {
    let root = page_container(stack, "dashboard", "Dashboard");
    let banner = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .build();
    root.append(&banner);

    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(12)
        .column_homogeneous(true)
        .build();
    let (f1, v_cpu) = tile("CPU usage");
    let (f2, v_power) = tile("Package power");
    let (f3, v_temp) = tile("Hottest CPU core");
    let (f4, v_gpu) = tile("GPU freq (act/max)");
    let (f5, v_ram) = tile("Memory used");
    let (f6, v_fan) = tile("Fan");
    let (f7, v_profile) = tile("Power profile");
    let (f8, v_peak) = tile("Session peak power");
    grid.attach(&f1, 0, 0, 1, 1);
    grid.attach(&f2, 1, 0, 1, 1);
    grid.attach(&f3, 0, 1, 1, 1);
    grid.attach(&f4, 1, 1, 1, 1);
    grid.attach(&f5, 0, 2, 1, 1);
    grid.attach(&f6, 1, 2, 1, 1);
    grid.attach(&f7, 0, 3, 1, 1);
    grid.attach(&f8, 1, 3, 1, 1);
    root.append(&grid);
    root.append(&note(
        "Readings come straight from kernel interfaces; sampling gaps (suspend, \
         driver hiccups) are reported honestly rather than guessed. Delta-based \
         values need one valid interval after startup.",
    ));

    DashRefs {
        banner,
        tile_cpu: v_cpu,
        tile_power: v_power,
        tile_temp: v_temp,
        tile_gpu: v_gpu,
        tile_ram: v_ram,
        tile_fan: v_fan,
        tile_profile: v_profile,
        tile_peak: v_peak,
    }
}

// ------------------------------------------------------------ sensors

struct SensorsRefs {
    temp_rows: gtk::ListBox,
    fan_rows: gtk::ListBox,
    row_map: std::collections::HashMap<String, adw::ActionRow>,
}

fn build_sensors(stack: &gtk::Stack) -> SensorsRefs {
    let root = page_container(stack, "sensors", "Sensors");
    root.append(&section_title("Temperatures"));
    let temp_rows = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    root.append(&temp_rows);
    root.append(&section_title("Fans"));
    let fan_rows = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    root.append(&fan_rows);
    root.append(&note(
        "Sensor labels and drivers as reported by the kernel. Fault flags come \
         from the driver's fanN_alarm input when it exposes one.",
    ));
    SensorsRefs {
        temp_rows,
        fan_rows,
        row_map: Default::default(),
    }
}

fn sensor_row(id: &str, label: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(label).subtitle(id).build()
}

// ------------------------------------------------------------ power

struct PowerRefs {
    active: gtk::Label,
    combo: adw::ComboRow,
    epp: gtk::Label,
    turbo: gtk::Label,
    rapl: gtk::Label,
    known_profiles: Vec<String>,
    user_touched: Rc<Cell<bool>>,
    syncing: Rc<Cell<bool>>,
}

fn build_power(stack: &gtk::Stack, timers: &mut Vec<glib::SourceId>) -> PowerRefs {
    let root = page_container(stack, "power", "Power");
    root.append(&section_title("Platform profile (power-profiles-daemon)"));
    let active = gtk::Label::builder()
        .label("Unknown — daemon not queried yet")
        .xalign(0.0)
        .build();
    root.append(&active);

    let combo = adw::ComboRow::builder()
        .title("Profile")
        .subtitle("")
        .build();
    let combo_list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    combo_list.append(&combo);
    root.append(&combo_list);
    let apply = gtk::Button::builder()
        .label("Switch profile")
        .css_classes(["suggested-action"])
        .halign(gtk::Align::Start)
        .build();
    root.append(&apply);
    let feedback = note("");
    feedback.set_visible(false);
    root.append(&feedback);

    root.append(&section_title("CPU policy (reported read-only)"));
    let epp = gtk::Label::builder()
        .label("EPP: waiting …")
        .xalign(0.0)
        .build();
    let turbo = gtk::Label::builder()
        .label("Turbo: waiting …")
        .xalign(0.0)
        .build();
    root.append(&epp);
    root.append(&turbo);

    root.append(&section_title(
        "RAPL energy domains (reported separately, never summed)",
    ));
    let rapl = gtk::Label::builder()
        .label("Unavailable")
        .wrap(true)
        .xalign(0.0)
        .build();
    root.append(&rapl);
    root.append(&note(
        "Power figures derive from the kernel RAPL energy counters between \
         samples; they cover package domains, not the whole system.",
    ));

    // apply button drives the only "write": a polkit-authorized switch that
    // is verified by re-reading the daemon's answer.
    let user_touched = Rc::new(Cell::new(false));
    let syncing = Rc::new(Cell::new(false));
    {
        let touched = user_touched.clone();
        let syncing = syncing.clone();
        combo.connect_selected_notify(move |_| {
            if !syncing.get() {
                touched.set(true);
            }
        });
    }
    let combo_c2 = combo.clone();
    // Worker thread does the D-Bus round-trip; feedback is queued and the
    // main thread drains it on a short timer (GTK widgets are not Send).
    let feedback_queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    {
        let q = feedback_queue.clone();
        let fb = feedback.clone();
        let touched = user_touched.clone();
        let button = apply.clone();
        let selection = combo.clone();
        timers.push(glib::timeout_add_local(
            std::time::Duration::from_millis(150),
            move || {
                loop {
                    let item = q.lock().ok().and_then(|mut v| v.pop_front());
                    match item {
                        Some(text) => {
                            touched.set(false); // re-sync combo selection afterwards
                            button.set_sensitive(true);
                            selection.set_sensitive(true);
                            fb.set_visible(true);
                            fb.set_text(&text);
                        }
                        None => break,
                    }
                }
                ControlFlow::Continue
            },
        ));
    }
    {
        let touched_c = user_touched.clone();
        let q_c = feedback_queue.clone();
        let combo_c = combo_c2.clone();
        let fb = feedback.clone();
        apply.connect_clicked(move |button| {
            if !button.is_sensitive() {
                return;
            }
            let Some(name) = combo_c.selected_item().and_then(|o| {
                o.downcast_ref::<gtk::StringObject>()
                    .map(|s| s.string().to_string())
            }) else {
                return;
            };
            touched_c.set(true);
            button.set_sensitive(false);
            combo_c.set_sensitive(false);
            fb.set_visible(true);
            fb.set_text(&format!("switching profile to '{name}' …"));
            let q = q_c.clone();
            std::thread::spawn(move || {
                let text = match nuc_power::ProfileClient::new()
                    .and_then(|client| client.set_profile_arg(&name))
                {
                    Ok(s) => format!(
                        "done — active profile now: {}",
                        s.active.unwrap_or_else(|| "unavailable".into())
                    ),
                    Err(e) => format!("switch failed: {e}"),
                };
                if let Ok(mut v) = q.lock() {
                    v.push_back(text);
                }
            });
        });
    }
    PowerRefs {
        active,
        combo,
        epp,
        turbo,
        rapl,
        known_profiles: Vec::new(),
        user_touched,
        syncing,
    }
}

// ------------------------------------------------------------ history page

struct HistoryRefs {
    chart: gtk::DrawingArea,
    status: gtk::Label,
}

fn build_history(state: Arc<Mutex<GuiState>>, stack: &gtk::Stack) -> HistoryRefs {
    let root = page_container(stack, "history", "History");
    root.append(&section_title("Live session (this window)"));
    let legend = note("— temperature/red · power/blue · utilization/green (gaps: unavailable) —");
    root.append(&legend);

    let chart = gtk::DrawingArea::builder()
        .content_height(300)
        .hexpand(true)
        .css_classes(["card"])
        .build();
    let chart_state = state.clone();
    chart.set_draw_func(move |_area, cr, w, h| {
        let series = match chart_state.lock() {
            Ok(st) => {
                let s = &st.series;
                (
                    s.cpu_temp.clone(),
                    s.pkg_power.clone(),
                    s.cpu_util.clone(),
                    s.start_ms,
                )
            }
            Err(_) => Default::default(),
        };
        chart::draw(cr, w, h, &series.0, &series.1, &series.2, series.3);
    });
    root.append(&chart);

    root.append(&section_title("Database (1-minute aggregates)"));
    let status = note("…");
    root.append(&status);
    root.append(&note(
        "Raw samples are retained for the configured hours, minute aggregates \
         for days. Nothing is recorded outside the NUC-XDG data directory.",
    ));
    HistoryRefs { chart, status }
}

// ------------------------------------------------------------ diagnostics

/// Messages the diagnostics worker thread feeds back to the main thread.
enum DiagMsg {
    Started(&'static str),
    Failed(String),
    Done {
        report: nuc_diagnostics::DiagnosticReport,
        export_note: Option<String>,
    },
}

fn build_diagnostics(
    state: Arc<Mutex<GuiState>>,
    stack: &gtk::Stack,
    timers: &mut Vec<glib::SourceId>,
) {
    let root = page_container(stack, "diagnostics", "Diagnostics");
    root.append(&section_title("Read-only sanity checks"));
    let busy = note("");
    busy.set_visible(false);
    root.append(&busy);
    let list = gtk::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    root.append(&list);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let run = gtk::Button::builder()
        .label("Run checks")
        .css_classes(["suggested-action"])
        .halign(gtk::Align::Start)
        .build();
    let export = gtk::Button::builder().label("Export report").build();
    actions.append(&run);
    actions.append(&export);
    root.append(&actions);
    root.append(&note(
        "Checks never write kernel state; they query sensors, D-Bus services \
         and the installed unit states only.",
    ));

    let msgs: Arc<Mutex<VecDeque<DiagMsg>>> = Arc::new(Mutex::new(VecDeque::new()));
    let running = Rc::new(Cell::new(false));

    // main-thread drain: rebuilds rows, updates the busy line
    {
        let q = msgs.clone();
        let list = list.clone();
        let busy = busy.clone();
        let running = running.clone();
        timers.push(glib::timeout_add_local(
            std::time::Duration::from_millis(150),
            move || {
                while let Some(m) = q.lock().ok().and_then(|mut v| v.pop_front()) {
                    match m {
                        DiagMsg::Started(text) => {
                            busy.set_visible(true);
                            busy.set_text(text);
                        }
                        DiagMsg::Failed(e) => {
                            busy.set_visible(true);
                            busy.set_text(&format!("diagnostics error: {e}"));
                            running.set(false);
                        }
                        DiagMsg::Done {
                            report,
                            export_note,
                        } => {
                            rebuild_diag_rows(&list, &report.checks);
                            busy.set_visible(true);
                            busy.set_text(&match export_note {
                                Some(note) => format!("{} — {}", report.verdict(), note),
                                None => report.verdict(),
                            });
                            running.set(false);
                        }
                    }
                }
                ControlFlow::Continue
            },
        ));
    }

    // shared job starter for both buttons (worker never touches widgets)
    fn start_job(
        state: &Arc<Mutex<GuiState>>,
        running: &Rc<Cell<bool>>,
        msgs: &Arc<Mutex<VecDeque<DiagMsg>>>,
        export: bool,
    ) {
        if running.get() {
            return;
        }
        running.set(true);
        let cfg = state.lock().ok().map(|s| s.cfg.clone());
        let Some(cfg) = cfg else {
            running.set(false);
            return;
        };
        let label = if export {
            "collecting + exporting report …"
        } else {
            "running checks … (a few seconds)"
        };
        if let Ok(mut v) = msgs.lock() {
            v.push_back(DiagMsg::Started(label));
        }
        let q = msgs.clone();
        std::thread::spawn(move || {
            let paths = nuc_monitor::fs::SourcePaths::system();
            let result = nuc_diagnostics::run(&paths, &cfg, "live");
            let msg = match result {
                Ok(report) if export => {
                    let note = {
                        let dir = nuc_core::config::data_dir();
                        let md = dir.join("diagnostics-report.md");
                        let js = dir.join("diagnostics-report.json");
                        std::fs::create_dir_all(&dir)
                            .ok()
                            .and_then(|_| std::fs::write(&md, report.export_markdown()).ok())
                            .and_then(|_| {
                                serde_json::to_string_pretty(&report)
                                    .ok()
                                    .and_then(|json| std::fs::write(&js, json).ok())
                            })
                            .map(|_| format!("exported {} · {}", md.display(), js.display()))
                            .or_else(|| {
                                Some(
                                    "export failed: could not write into the data directory"
                                        .to_string(),
                                )
                            })
                    };
                    DiagMsg::Done {
                        report,
                        export_note: note,
                    }
                }
                Ok(report) => DiagMsg::Done {
                    report,
                    export_note: None,
                },
                Err(e) => DiagMsg::Failed(e.to_string()),
            };
            if let Ok(mut v) = q.lock() {
                v.push_back(msg);
            }
        });
    }

    {
        let running = running.clone();
        let msgs = msgs.clone();
        let state = state.clone();
        run.connect_clicked(move |_| start_job(&state, &running, &msgs, false));
    }
    {
        let running = running.clone();
        let msgs = msgs.clone();
        export.connect_clicked(move |_| start_job(&state, &running, &msgs, true));
    }
}

fn rebuild_diag_rows(list: &gtk::ListBox, checks: &[nuc_diagnostics::Check]) {
    clear_box(list);
    for c in checks {
        let row = adw::ActionRow::builder()
            .title(format!("{} — {}", c.category, c.id))
            .subtitle(&c.detail)
            .build();
        row.add_css_class(match c.status {
            nuc_diagnostics::Status::Ok => "success",
            nuc_diagnostics::Status::Warning => "warning",
            nuc_diagnostics::Status::Fail => "error",
            nuc_diagnostics::Status::Info => "",
            nuc_diagnostics::Status::Unavailable => "dim-label",
        });
        list.append(&row);
    }
}

// ------------------------------------------------------------ settings

fn spin(v: f64, lo: f64, hi: f64, step: f64) -> gtk::SpinButton {
    let adj = gtk::Adjustment::new(v, lo, hi, step, step * 10.0, 0.0);
    gtk::SpinButton::new(Some(&adj), 1.0, 1)
}

fn build_settings(state: Arc<Mutex<GuiState>>, stack: &gtk::Stack) {
    let root = page_container(stack, "settings", "Settings");
    root.append(&section_title("Monitoring"));
    let cfg = state.lock().map(|s| s.cfg.clone()).unwrap_or_default();

    let interval_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .build();
    interval_grid.attach(
        &gtk::Label::builder()
            .label("Sample interval (ms)")
            .halign(gtk::Align::End)
            .build(),
        0,
        0,
        1,
        1,
    );
    let interval = spin(cfg.monitoring.interval_ms as f64, 1000.0, 60_000.0, 500.0);
    interval_grid.attach(&interval, 1, 0, 1, 1);
    interval_grid.attach(
        &gtk::Label::builder()
            .label("GPU monitoring")
            .halign(gtk::Align::End)
            .build(),
        0,
        1,
        1,
        1,
    );
    let gpu_enabled = gtk::Switch::builder()
        .state(cfg.monitoring.gpu_enabled)
        .active(cfg.monitoring.gpu_enabled)
        .build();
    interval_grid.attach(&gpu_enabled, 1, 1, 1, 1);
    interval_grid.attach(
        &gtk::Label::builder()
            .label("Record samples to history DB")
            .halign(gtk::Align::End)
            .build(),
        0,
        2,
        1,
        1,
    );
    let record = gtk::Switch::builder()
        .state(cfg.monitoring.record)
        .active(cfg.monitoring.record)
        .build();
    interval_grid.attach(&record, 1, 2, 1, 1);
    root.append(&interval_grid);

    root.append(&section_title("Alert thresholds"));
    let t_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .build();
    t_grid.attach(
        &gtk::Label::builder()
            .label("CPU warm (°C)")
            .halign(gtk::Align::End)
            .build(),
        0,
        0,
        1,
        1,
    );
    let cpu_warm = spin(cfg.alerts.cpu_temp_warm_c.unwrap_or(85.0), 40.0, 120.0, 1.0);
    t_grid.attach(&cpu_warm, 1, 0, 1, 1);
    t_grid.attach(
        &gtk::Label::builder()
            .label("NVMe warm (°C)")
            .halign(gtk::Align::End)
            .build(),
        0,
        1,
        1,
        1,
    );
    let nvme_warm = spin(
        cfg.alerts.nvme_temp_warm_c.unwrap_or(70.0),
        30.0,
        100.0,
        1.0,
    );
    t_grid.attach(&nvme_warm, 1, 1, 1, 1);
    t_grid.attach(
        &gtk::Label::builder()
            .label("Hysteresis (°C)")
            .halign(gtk::Align::End)
            .build(),
        0,
        2,
        1,
        1,
    );
    let hysteresis = spin(cfg.alerts.hysteresis_c, 0.5, 10.0, 0.5);
    t_grid.attach(&hysteresis, 1, 2, 1, 1);
    t_grid.attach(
        &gtk::Label::builder()
            .label("Debounce (samples)")
            .halign(gtk::Align::End)
            .build(),
        0,
        3,
        1,
        1,
    );
    let above_samples = spin(cfg.alerts.above_samples as f64, 1.0, 30.0, 1.0);
    t_grid.attach(&above_samples, 1, 3, 1, 1);
    root.append(&t_grid);
    root.append(&note(
        "Alert thresholds act as advisory warnings only; no kernel thermal \
         protection is ever modified. New thresholds apply to the alert engine \
         on restart.",
    ));

    let save = gtk::Button::builder()
        .label("Save settings")
        .css_classes(["suggested-action"])
        .halign(gtk::Align::Start)
        .build();
    let feedback = note("");
    feedback.set_visible(false);
    root.append(&save);
    root.append(&feedback);

    let (interval_s, gpu_s, record_s, cwarm_s, nwarm_s, hyst_s, above_s, fb_s) = (
        interval.clone(),
        gpu_enabled.clone(),
        record.clone(),
        cpu_warm.clone(),
        nvme_warm.clone(),
        hysteresis.clone(),
        above_samples.clone(),
        feedback.clone(),
    );
    let state_save = state.clone();
    save.connect_clicked(move |_| {
        let mut st = match state_save.lock() {
            Ok(st) => st,
            Err(_) => return,
        };
        // SpinButton ranges are bounded and positive; truncation is intended.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            st.cfg.monitoring.interval_ms = interval_s.value() as u64;
            st.cfg.alerts.above_samples = above_s.value() as u32;
        }
        st.cfg.monitoring.gpu_enabled = gpu_s.is_active();
        st.cfg.monitoring.record = record_s.is_active();
        st.cfg.alerts.cpu_temp_warm_c = Some(cwarm_s.value());
        st.cfg.alerts.nvme_temp_warm_c = Some(nwarm_s.value());
        st.cfg.alerts.hysteresis_c = hyst_s.value();
        st.handle.set_interval(std::time::Duration::from_millis(
            st.cfg.monitoring.interval_ms,
        ));
        st.handle.set_gpu_enabled(st.cfg.monitoring.gpu_enabled);
        let text = match st.cfg.save() {
            Ok(()) => format!("saved to {}", nuc_core::config::config_path().display()),
            Err(e) => format!("save failed: {e}"),
        };
        fb_s.set_visible(true);
        fb_s.set_text(&text);
    });
}

// ----------------------------------------------------------- THE TICK

fn tick(app: &adw::Application, st: &mut GuiState, ui: &mut Ui) {
    // 1) record + notifications + housekeeping (guarded by the state lock)
    if let Some(snap) = st.handle.latest() {
        let new_sample = st.series.push(&snap);
        if new_sample
            && st.cfg.monitoring.record
            && let Err(error) = st.history.record(snap.clone())
        {
            ui.history.status.set_text(error);
        }
        st.tick_count += 1;
        if st.tick_count.is_multiple_of(20) {
            st.history
                .maintain(st.cfg.history.clone(), st.tick_count.is_multiple_of(60));
        }
        if let Some(status) = st.history.latest_status() {
            ui.history.status.set_text(&status);
        }
        // relay queued alerts to desktop notifications
        while let Ok(ev) = st.controls.alerts.try_recv() {
            show_notification(app, &ev);
        }
        // 2) push values to widgets
        update_dash(ui, &snap, &st.cfg.general.temp_unit);
        update_sensors(ui, &snap, &st.cfg.general.temp_unit);
        update_power(ui, &snap);
        if st
            .tick_count
            .is_multiple_of((st.cfg.general.chart_refresh_ms / 1000).max(1))
        {
            ui.history.chart.queue_draw();
        }
    }
}

fn update_dash(ui: &mut Ui, snap: &Snapshot, unit: &TempUnit) {
    ui.dash.tile_cpu.set_text(
        &snap
            .cpu
            .usage_total
            .map(|u| format!("{u:.1}%"))
            .unwrap_or_else(|| "Unavailable (first interval)".into()),
    );
    ui.dash.tile_power.set_text(
        &snap
            .cpu
            .interval_power_w
            .map(|w| format!("{w:.2} W"))
            .unwrap_or_else(|| "Unavailable (first interval)".into()),
    );
    let hottest = snap
        .temperatures
        .iter()
        .filter(|t| t.source.contains("coretemp"))
        .filter_map(|t| t.value_c)
        .fold(None::<f64>, |a, v| a.map_or(Some(v), |a| Some(a.max(v))));
    match hottest {
        Some(c) => {
            ui.dash.tile_temp.set_text(&unit.format(c));
        }
        None => ui.dash.tile_temp.set_text("Unavailable"),
    }
    match &snap.gpu {
        Some(g) => ui.dash.tile_gpu.set_text(&format!(
            "{}/{} MHz",
            g.act_freq_mhz
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".into()),
            g.max_freq_mhz
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".into()),
        )),
        None => ui.dash.tile_gpu.set_text("Unavailable"),
    }
    let ram = match (snap.memory.used_kib, snap.memory.total_kib) {
        (Some(u), Some(t)) if t > 0 => {
            nuc_core::units::fmt_bytes(u * 1024) + &format!(" ({} %)", u * 100 / t)
        }
        _ => "Unavailable".into(),
    };
    ui.dash.tile_ram.set_text(&ram);
    let fan = snap.fans.first();
    let fan_txt = match fan {
        Some(f) => format!(
            "{}{}",
            f.formatted_speed(),
            match f.fault {
                Some(true) => "  [FAULT]",
                Some(false) => "",
                None => "",
            }
        ),
        None => "none detected".into(),
    };
    ui.dash.tile_fan.set_text(&fan_txt);
    match &snap.profile {
        Some(p) => {
            let marker = if p.degraded { " (degraded)" } else { "" };
            ui.dash.tile_profile.set_text(&format!(
                "{}{marker}",
                p.active.clone().unwrap_or_else(|| "Unknown".into())
            ));
        }
        None => ui.dash.tile_profile.set_text("Unavailable"),
    }
    ui.dash.tile_peak.set_text(
        &snap
            .peak_power_w
            .map(|w| format!("{w:.2} W"))
            .unwrap_or_else(|| "Unavailable".into()),
    );
}

fn update_sensors(ui: &mut Ui, snap: &Snapshot, unit: &TempUnit) {
    let ids: std::collections::HashSet<String> = snap
        .temperatures
        .iter()
        .map(|t| format!("temp/{}", t.id))
        .chain(snap.fans.iter().map(|f| format!("fan/{}", f.id)))
        .collect();
    ui.sensors.row_map.retain(|id, row| {
        if ids.contains(id) {
            return true;
        }
        if id.starts_with("fan/") {
            ui.sensors.fan_rows.remove(row);
        } else {
            ui.sensors.temp_rows.remove(row);
        }
        false
    });
    for t in &snap.temperatures {
        let row = ui
            .sensors
            .row_map
            .entry(format!("temp/{}", t.id))
            .or_insert_with(|| {
                let row = sensor_row(&t.id, &t.label);
                ui.sensors.temp_rows.append(&row);
                row
            });
        let value = fmt_temp(t.value_c, *unit);
        let limits = match (t.warn_c, t.crit_c) {
            (Some(w), Some(c)) => format!(" · warn {} / crit {}", unit.format(w), unit.format(c)),
            (Some(w), None) => format!(" · warn {}", unit.format(w)),
            (None, Some(c)) => format!(" · crit {}", unit.format(c)),
            _ => String::new(),
        };
        let alarm = if t.alarm { " · ALARM" } else { "" };
        row.set_subtitle(&format!("{value}{limits}{alarm}  [{}]", t.source));
    }
    for f in &snap.fans {
        let key = format!("fan/{}", f.id);
        let row = ui.sensors.row_map.entry(key).or_insert_with(|| {
            let row = sensor_row(&f.id, &f.label);
            ui.sensors.fan_rows.append(&row);
            row
        });
        let rpm = f.formatted_speed();
        let fault = match f.fault {
            Some(true) => " · FAULT",
            Some(false) => "",
            None => "",
        };
        let note = f
            .note
            .clone()
            .map(|n| format!(" · {n}"))
            .unwrap_or_default();
        row.set_subtitle(&format!("{rpm}{fault}{note}  [{}]", f.source));
    }
}

fn update_power(ui: &mut Ui, snap: &Snapshot) {
    if let Some(p) = &snap.profile {
        ui.power.syncing.set(true);
        ui.power.active.set_text(&format!(
            "active: {} · available: {:?}{}",
            p.active.clone().unwrap_or_else(|| "Unknown".into()),
            p.profiles,
            p.degraded_reason
                .clone()
                .map(|r| format!("  · degraded: {r}"))
                .unwrap_or_default()
        ));
        // keep user's pending selection; reflect the daemon when untouched
        if !ui.power.user_touched.get() && ui.power.known_profiles != p.profiles {
            let names: Vec<&str> = p.profiles.iter().map(|s| s.as_str()).collect();
            let list = gtk::StringList::new(&names);
            ui.power.combo.set_model(Some(&list));
            ui.power.known_profiles = p.profiles.clone();
        }
        if !ui.power.user_touched.get() {
            for (i, name) in p.profiles.iter().enumerate() {
                if Some(name.as_str()) == p.active.as_deref() {
                    ui.power.combo.set_selected(u32::try_from(i).unwrap_or(0));
                }
            }
        }
        ui.power.syncing.set(false);
    } else {
        ui.power
            .active
            .set_text("Unavailable — power-profiles-daemon did not answer");
    }
    let c = &snap.cpu;
    ui.power.epp.set_text(&format!(
        "EPP: {}  · governor: {}  · driver: {}{}",
        c.freq.epp.clone().unwrap_or_else(|| "Unavailable".into()),
        c.freq
            .governor
            .clone()
            .unwrap_or_else(|| "Unavailable".into()),
        if c.freq.running_driver.is_empty() {
            "n/a".to_string()
        } else {
            c.freq.running_driver.clone()
        },
        c.freq
            .intel_pstate_mode
            .clone()
            .map(|m| format!(" ({m} mode)"))
            .unwrap_or_default()
    ));
    ui.power.turbo.set_text(&format!(
        "Turbo: {}",
        match c.freq.turbo_enabled {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "Unavailable",
        }
    ));
    if snap.rapl.is_empty() {
        ui.power.rapl.set_text("Unavailable");
    } else {
        let mut txt = String::new();
        for z in &snap.rapl {
            txt.push_str(&format!(
                "{} · {} {} W\n",
                z.id,
                z.name,
                z.power_w
                    .map(|w| format!("{w:.2}"))
                    .unwrap_or_else(|| "n/a".into())
            ));
        }
        ui.power.rapl.set_text(txt.trim_end());
    }
}

fn show_notification(app: &adw::Application, ev: &AlertEvent) {
    let kind = format!("{:?}", ev.kind).to_lowercase();
    let n = gtk::gio::Notification::new(&format!("NUC Control — {kind}"));
    n.set_body(Some(&ev.message));
    n.set_priority(match ev.level.as_str() {
        "critical" | "fail" => gtk::gio::NotificationPriority::High,
        _ => gtk::gio::NotificationPriority::Normal,
    });
    app.send_notification(Some("nuc-progress"), &n);
}
