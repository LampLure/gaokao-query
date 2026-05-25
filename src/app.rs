use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;
use crate::data::*;
use crate::matcher;
use crate::parser;
// prediction module is used via crate::prediction:: calls in async tasks

#[derive(Clone, PartialEq)]
enum QueryState {
    Idle,
    Running,
    Paused,
}

pub struct GaokaoApp {
    // config persistence
    config: AppConfig,
    config_dirty: bool,
    // scene
    scene: Scene,
    target_url: String,
    // ---- 考场查询 scene ----
    baokao_path: Option<String>,
    sfz_path: Option<String>,
    matched_records: Vec<MatchedRecord>,
    results: Arc<Mutex<Vec<QueryResult>>>,
    displayed_results: Vec<QueryResult>,
    query_state: QueryState,
    // ---- 报考号推算 scene ----
    pred_sfz_path: Option<String>,
    pred_bkh_path: Option<String>,
    pred_sfz_records: Vec<ShenFenZhengRecord>,
    pred_known_bkh: Vec<BaoKaoHaoRecord>,
    pred_year_filter: f64,
    pred_start_str: String,          // 开始考号输入框（完整14位如 26421126151462）
    pred_start_bkh: u64,             // 开始考号完整数值（14位数字）
    pred_end_str: String,            // 结束考号输入框（完整14位如 26421126151500）
    pred_end_bkh: u64,               // 结束考号完整数值（14位数字）
    pred_search_margin: u32,         // 网格探针数量
    pred_results: Arc<Mutex<Vec<PredictedRecord>>>,
    pred_displayed_results: Vec<PredictedRecord>,
    pred_state: QueryState,
    pred_continuous: bool,
    // ---- shared params ----
    concurrency: u32,
    delay_ms: u32,
    step_delay_ms: u32,
    captcha_retries: u32,
    captcha_wait_ms: u32,
    // shared progress
    progress: Arc<Mutex<QueryProgress>>,
    pred_progress: Arc<Mutex<PredictionProgress>>,
    status_message: String,
    // debug
    debug_mode: bool,
    show_perf: bool,
    hide_browser: bool,
    debug_logs: Arc<Mutex<Vec<String>>>,
    displayed_logs: Vec<String>,
    // performance profiling
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
    perf_stats: Vec<PerfRecord>,
    captcha_stats: Arc<Mutex<CaptchaStats>>,
    browser_statuses: Arc<Mutex<Vec<BrowserStatus>>>,
    displayed_browser_statuses: Vec<BrowserStatus>,
    // cancellation
    cancel_flag: Arc<Mutex<bool>>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryProgress {
    pub total: usize,
    pub completed: usize,
    pub success: usize,
    pub failed: usize,
    pub current_name: String,
}

impl GaokaoApp {
    pub fn new() -> Self {
        let cfg = config::load();
        let mut app = Self {
            baokao_path: if cfg.baokao_path.is_empty() { None } else { Some(cfg.baokao_path.clone()) },
            sfz_path: if cfg.sfz_path.is_empty() { None } else { Some(cfg.sfz_path.clone()) },
            config: cfg.clone(),
            config_dirty: false,
            scene: Scene::ExamRoomQuery,
            target_url: cfg.target_url.clone(),
            matched_records: Vec::new(),
            results: Arc::new(Mutex::new(Vec::new())),
            displayed_results: Vec::new(),
            query_state: QueryState::Idle,
            // prediction
            pred_sfz_path: if cfg.pred_sfz_path.is_empty() { None } else { Some(cfg.pred_sfz_path.clone()) },
            pred_bkh_path: if cfg.pred_bkh_path.is_empty() { None } else { Some(cfg.pred_bkh_path.clone()) },
            pred_sfz_records: Vec::new(),
            pred_known_bkh: Vec::new(),
            pred_year_filter: 2023.0,
            pred_start_str: cfg.pred_start_bkh.clone(),
            pred_start_bkh: cfg.pred_start_bkh.parse().unwrap_or(0),
            pred_end_str: cfg.pred_end_bkh.clone(),
            pred_end_bkh: cfg.pred_end_bkh.parse().unwrap_or(0),
            pred_search_margin: 10,
            pred_results: Arc::new(Mutex::new(Vec::new())),
            pred_displayed_results: Vec::new(),
            pred_state: QueryState::Idle,
            pred_continuous: false,
            concurrency: cfg.concurrency,
            delay_ms: cfg.delay_ms,
            step_delay_ms: cfg.step_delay_ms,
            captcha_retries: cfg.captcha_retries,
            captcha_wait_ms: cfg.captcha_wait_ms,
            progress: Arc::new(Mutex::new(QueryProgress::default())),
            pred_progress: Arc::new(Mutex::new(PredictionProgress::default())),
            status_message: String::new(),
            debug_mode: cfg.debug_mode,
            show_perf: false,
            hide_browser: cfg.hide_browser,
            debug_logs: Arc::new(Mutex::new(Vec::new())),
            displayed_logs: Vec::new(),
            perf_logs: Arc::new(Mutex::new(Vec::new())),
            perf_stats: Vec::new(),
            captcha_stats: Arc::new(Mutex::new(CaptchaStats::default())),
            browser_statuses: Arc::new(Mutex::new(Vec::new())),
            displayed_browser_statuses: Vec::new(),
            cancel_flag: Arc::new(Mutex::new(false)),
        };
        if app.baokao_path.is_some() && app.sfz_path.is_some() {
            app.parse_and_match();
        }
        if let Some(p) = &app.pred_sfz_path {
            if let Ok(records) = parser::parse_shenfenzheng(p) {
                app.pred_sfz_records = records;
            }
        }
        if let Some(p) = &app.pred_bkh_path {
            let _ = parser::parse_baokao_hao(p).map(|r| app.pred_known_bkh = r);
        }
        app
    }
}

impl GaokaoApp {
    fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        if let Ok(mut logs) = self.debug_logs.try_lock() {
            logs.push(format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg));
        }
    }

    fn auto_save_config(&mut self) {
        if !self.config_dirty { return; }
        self.config.baokao_path = self.baokao_path.as_ref().cloned().unwrap_or_default();
        self.config.sfz_path = self.sfz_path.as_ref().cloned().unwrap_or_default();
        self.config.pred_sfz_path = self.pred_sfz_path.as_ref().cloned().unwrap_or_default();
        self.config.pred_bkh_path = self.pred_bkh_path.as_ref().cloned().unwrap_or_default();
        self.config.pred_start_bkh = self.pred_start_str.clone();
        self.config.pred_end_bkh = self.pred_end_str.clone();
        self.config.target_url = self.target_url.clone();
        self.config.concurrency = self.concurrency;
        self.config.delay_ms = self.delay_ms;
        self.config.step_delay_ms = self.step_delay_ms;
        self.config.captcha_retries = self.captcha_retries;
        self.config.captcha_wait_ms = self.captcha_wait_ms;
        self.config.hide_browser = self.hide_browser;
        self.config.debug_mode = self.debug_mode;
        self.config.turbo = self.config.turbo;
        config::save(&self.config);
        self.config_dirty = false;
    }

    // ---- 统计指定年份人数 ----
    fn count_students_for_year(&self, year: u32) -> usize {
        self.pred_sfz_records.iter()
            .filter(|r| r.ruxue_year.unwrap_or(0.0) as u32 == year)
            .count()
    }

    /// 从用户输入中解析考号数值
    /// 直接解析为 u64 数字，用于后续 ±偏移运算
    /// 例如："26421126151462" → 26421126151462
    fn parse_bkh_number(input: &str) -> u64 {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return 0;
        }
        trimmed.parse::<u64>().unwrap_or(0)
    }
}

impl eframe::App for GaokaoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // sync results
        if let Ok(r) = self.results.try_lock() {
            if r.len() != self.displayed_results.len() {
                let mut sorted = r.clone();
                sorted.sort_by(|a, b| a.baominghao.cmp(&b.baominghao));
                self.displayed_results = sorted;
            }
        }
        if let Ok(r) = self.pred_results.try_lock() {
            if r.len() != self.pred_displayed_results.len() {
                self.pred_displayed_results = r.clone();
            }
        }
        if let Ok(l) = self.debug_logs.try_lock() {
            if l.len() != self.displayed_logs.len() {
                self.displayed_logs = l.clone();
            }
        }
        if let Ok(bs) = self.browser_statuses.try_lock() {
            if bs.len() != self.displayed_browser_statuses.len() || bs.iter().zip(self.displayed_browser_statuses.iter()).any(|(a, b)| a.step != b.step || a.target != b.target || a.name != b.name || a.captcha_attempt != b.captcha_attempt) {
                self.displayed_browser_statuses = bs.clone();
            }
        }

        // aggregate perf logs into stats
        if let Ok(pl) = self.perf_logs.try_lock() {
            if !pl.is_empty() {
                let mut stats: std::collections::HashMap<&'static str, Vec<u64>> = std::collections::HashMap::new();
                for record in pl.iter() {
                    for event in record {
                        stats.entry(event.label).or_default().push(event.elapsed_ms);
                    }
                }
                let mut new_stats: Vec<PerfRecord> = stats.into_iter()
                    .map(|(label, times_ms)| PerfRecord { label, times_ms })
                    .collect();
                new_stats.sort_by(|a, b| a.label.cmp(b.label));
                self.perf_stats = new_stats;
            }
        }

        self.auto_save_config();

        // === sidebar ===
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(180.0)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    ui.label(egui::RichText::new("高考查询系统").heading().strong());
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label("场景导航");
                    ui.add_space(4.0);
                    for s in Scene::all() {
                        let selected = self.scene == s;
                        let text = if selected { format!("📌 {}", s.name()) } else { format!("  {}", s.name()) };
                        if ui.selectable_label(selected, text).clicked() && !selected {
                            self.scene = s;
                            self.log(format!("切换到场景: {}", self.scene.name()));
                        }
                    }
                    ui.separator();
                    ui.add_space(8.0);
                    if let Some(p) = &self.baokao_path {
                        ui.label(format!("📁 报名号: {}", p.split('/').last().unwrap_or("")));
                    }
                    if let Some(p) = &self.sfz_path {
                        ui.label(format!("📁 身份证: {}", p.split('/').last().unwrap_or("")));
                    }
                });
            });

        // === debug floating window ===
        if self.debug_mode {
            let logs = self.displayed_logs.clone();
            egui::Window::new("🔧 调试日志")
                .default_size([600.0, 400.0])
                .resizable(true)
                .vscroll(true)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().id_salt("debug_logs")
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &logs {
                                ui.label(line);
                            }
                        });
                });
        }

        // === performance window ===
        if self.show_perf {
            let stats = self.perf_stats.clone();
            egui::Window::new("⏱ 性能分析")
                .default_size([500.0, 300.0])
                .resizable(true)
                .vscroll(true)
                .show(ctx, |ui| {
                    if stats.is_empty() {
                        ui.label("暂无性能数据，开始查询后自动记录");
                    } else {
                        egui::ScrollArea::vertical().id_salt("perf_table").show(ui, |ui| {
                            egui::Grid::new("perf_grid").striped(true).num_columns(5).show(ui, |ui| {
                                ui.strong("操作"); ui.strong("次数"); ui.strong("平均(ms)"); ui.strong("最大(ms)"); ui.strong("最小(ms)");
                                ui.end_row();
                                for r in &stats {
                                    if r.count() == 0 { continue; }
                                    ui.label(r.label);
                                    ui.label(r.count().to_string());
                                    ui.label(format!("{:.0}", r.avg_ms()));
                                    ui.label(r.max_ms().to_string());
                                    ui.label(if r.min_ms() == u64::MAX { "0".into() } else { r.min_ms().to_string() });
                                    ui.end_row();
                                }
                            });
                        });
                    }
                });
        }

        // === central panel ===
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(self.scene.name());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let hw = self.hide_browser;
                    ui.checkbox(&mut self.hide_browser, "🙈 隐藏浏览器");
                    if hw != self.hide_browser { self.config_dirty = true; }
                    let dm = self.debug_mode;
                    ui.checkbox(&mut self.debug_mode, "🔧 调试模式");
                    if dm != self.debug_mode { self.config_dirty = true; }
                    let sp = self.show_perf;
                    ui.checkbox(&mut self.show_perf, "⏱ 性能");
                    if sp != self.show_perf { self.config_dirty = true; }
                    let tb = self.config.turbo;
                    ui.checkbox(&mut self.config.turbo, "🔥 暴力");
                    if tb != self.config.turbo { self.config_dirty = true; }
                    if let Ok(cs) = self.captcha_stats.try_lock() {
                        if cs.total_attempts > 0 {
                            ui.label(egui::RichText::new(format!(
                                "验证码 {}/{} ({:.0}%) 一次过:{}/{} ({:.0}%)",
                                cs.total_passes, cs.total_attempts, cs.pass_rate(),
                                cs.first_try_passes, cs.total_attempts, cs.first_try_rate()
                            )).color(if cs.pass_rate() >= 80.0 { egui::Color32::GREEN } else if cs.pass_rate() >= 50.0 { egui::Color32::YELLOW } else { egui::Color32::RED }).strong());
                        } else {
                            ui.label("验证码 --%");
                        }
                    }
                });
            });
            ui.separator();
            ui.add_space(4.0);

            match self.scene {
                Scene::ExamRoomQuery => self.ui_exam_room_query(ctx, ui),
                Scene::NumberPrediction => self.ui_number_prediction(ctx, ui),
            }

            if self.query_state == QueryState::Running || self.query_state == QueryState::Paused
                || self.pred_state == QueryState::Running
            {
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        });
    }
}

// ============================================================
// 考场查询 UI
// ============================================================
impl GaokaoApp {
    fn ui_exam_room_query(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        // URL input
        ui.horizontal(|ui| {
            ui.label("目标网址：");
            let mut url = self.target_url.clone();
            if ui.add_sized([ui.available_width() - 60.0, 0.0], egui::TextEdit::singleline(&mut url)).changed() {
                self.target_url = url;
                self.config_dirty = true;
            }
        });
        ui.add_space(8.0);

        egui::Grid::new("upload_grid").num_columns(3).striped(true).show(ui, |ui| {
            ui.label("报考号表格：");
            let fname = self.baokao_path.as_ref()
                .and_then(|p| p.split('/').last()).unwrap_or("未选择文件");
            ui.label(fname);
            if ui.button("📁 选择文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Excel", &["xlsx", "xls"]).pick_file()
                {
                    let s = path.to_string_lossy().to_string();
                    self.baokao_path = Some(s.clone());
                    self.config.baokao_path = s;
                    self.config_dirty = true;
                    self.log(format!("选择报考号表格: {}", self.baokao_path.as_ref().unwrap()));
                    self.parse_and_match();
                }
            }
            ui.end_row();

            ui.label("身份证和信息表格：");
            let fname2 = self.sfz_path.as_ref()
                .and_then(|p| p.split('/').last()).unwrap_or("未选择文件");
            ui.label(fname2);
            if ui.button("📁 选择文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Excel", &["xlsx", "xls"]).pick_file()
                {
                    let s = path.to_string_lossy().to_string();
                    self.sfz_path = Some(s.clone());
                    self.config.sfz_path = s;
                    self.config_dirty = true;
                    self.log(format!("选择身份证表格: {}", self.sfz_path.as_ref().unwrap()));
                    self.parse_and_match();
                }
            }
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // matched table
        if !self.matched_records.is_empty() {
            ui.push_id("matched_section", |ui| {
                ui.label(egui::RichText::new(format!(
                    "📋 匹配结果（共 {} 条）", self.matched_records.len()
                )).strong());
                ui.add_space(4.0);

                egui::ScrollArea::vertical().id_salt("matched_table").max_height(180.0).show(ui, |ui| {
                    TableBuilder::new(ui).id_salt("matched")
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .header(20.0, |mut h| {
                            h.col(|ui| { ui.label("姓名"); });
                            h.col(|ui| { ui.label("报名号"); });
                            h.col(|ui| { ui.label("身份证号"); });
                            h.col(|ui| { ui.label("报考信息"); });
                            h.col(|ui| { ui.label("状态"); });
                        })
                        .body(|body| {
                            body.rows(20.0, self.matched_records.len(), |mut row| {
                                let i = row.index();
                                if let Some(r) = self.matched_records.get(i) {
                                    row.col(|ui| { ui.label(&r.name); });
                                    row.col(|ui| { ui.label(&r.baominghao); });
                                    row.col(|ui| {
                                        let t = match &r.status {
                                            MatchStatus::Matched(s) => s.clone(),
                                            MatchStatus::Multiple => format!("同名{}人", r.shenfenzheng_candidates.len()),
                                            MatchStatus::NotFound => "未找到".into(),
                                            MatchStatus::Pending => "待匹配".into(),
                                        };
                                        ui.label(t);
                                    });
                                    row.col(|ui| { ui.label(&r.baokao_info); });
                                    row.col(|ui| {
                                        let (t, c) = match &r.status {
                                            MatchStatus::Matched(_) => ("已匹配", egui::Color32::GREEN),
                                            MatchStatus::Multiple => ("同名需穷举", egui::Color32::YELLOW),
                                            MatchStatus::NotFound => ("未找到", egui::Color32::RED),
                                            MatchStatus::Pending => ("待匹配", egui::Color32::GRAY),
                                        };
                                        ui.label(egui::RichText::new(t).color(c));
                                    });
                                }
                            });
                        });
                });
            });
        }

        ui.add_space(8.0);

        // query controls
        if !self.matched_records.is_empty() {
            if self.query_state != QueryState::Running {
                self.ui_query_params(ui);
                ui.add_space(8.0);
            }
            self.ui_query_buttons(ui, ctx);
        }

        if self.query_state == QueryState::Running {
            ui.push_id("pause_btn", |ui| {
                if ui.button(egui::RichText::new("⏸ 暂停").heading()).clicked() {
                    *self.cancel_flag.try_lock().unwrap() = true;
                    self.query_state = QueryState::Paused;
                    self.log("查询暂停");
                }
            });
        }

        self.ui_progress(ui);

        if !self.status_message.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(&self.status_message);
        }

        // results table
        if !self.displayed_results.is_empty() {
            self.ui_results_table(ui);
        }
    }

    fn ui_query_params(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("并发数：");
            ui.add(egui::Slider::new(&mut self.concurrency, 1..=10).text("个"));
            ui.add_space(16.0);
            ui.label("查询间隔：");
            ui.add(egui::Slider::new(&mut self.delay_ms, 0..=10000).text("毫秒").suffix("ms"));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("操作速度：");
            ui.add(egui::Slider::new(&mut self.step_delay_ms, 100..=5000).text("ms/步").suffix("ms"));
            ui.label(format!("({:.1}s)", self.step_delay_ms as f64 / 1000.0));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("验证码重试：");
            ui.add(egui::Slider::new(&mut self.captcha_retries, 1..=10).text("次"));
            ui.add_space(16.0);
            ui.label("首次等待：");
            ui.add(egui::Slider::new(&mut self.captcha_wait_ms, 500..=10000).text("ms").suffix("ms"));
        });
    }

    fn ui_query_buttons(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.query_state == QueryState::Idle {
            if ui.button(egui::RichText::new("▶ 开始查询").heading().color(egui::Color32::WHITE)).clicked() {
                self.start_query(ctx);
            }
        }
        if self.query_state == QueryState::Paused {
            ui.horizontal(|ui| {
                if ui.button("▶ 继续").clicked() {
                    *self.cancel_flag.try_lock().unwrap() = false;
                    self.query_state = QueryState::Running;
                    self.log("查询继续");
                    self.start_query(ctx);
                }
                if ui.button("⏹ 重新开始").clicked() {
                    *self.cancel_flag.try_lock().unwrap() = true;
                    self.query_state = QueryState::Idle;
                    self.log("查询已终止，准备重新开始");
                }
            });
        }
    }

    fn ui_progress(&mut self, ui: &mut egui::Ui) {
        ui.push_id("progress_section", |ui| {
            if let Ok(p) = self.progress.try_lock() {
                if p.total > 0 {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    let ratio = if p.total > 0 { p.completed as f32 / p.total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(ratio).text(format!("{}/{}", p.completed, p.total)));
                    ui.label(format!(
                        "✅ 成功: {}   ❌ 失败: {}   📌 当前: {}",
                        p.success, p.failed, p.current_name
                    ));
                }
            }
        });

        // 验证码统计面板
        self.ui_captcha_stats(ui);

        // 浏览器实时状态面板
        self.ui_browser_status(ui);
    }

    /// 验证码统计面板（独立方法，两个场景共用）
    fn ui_captcha_stats(&mut self, ui: &mut egui::Ui) {
        ui.push_id("captcha_stats_panel", |ui| {
            if let Ok(cs) = self.captcha_stats.try_lock() {
                if cs.total_attempts > 0 {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("🔐 验证码统计").strong());

                        let pass_rate = cs.pass_rate();
                        let first_rate = cs.first_try_rate();
                        let rate_color = if pass_rate >= 80.0 { egui::Color32::GREEN } else if pass_rate >= 50.0 { egui::Color32::YELLOW } else { egui::Color32::RED };

                        ui.label(egui::RichText::new(format!(
                            "通过 {}/{} ({:.0}%)",
                            cs.total_passes, cs.total_attempts, pass_rate
                        )).color(rate_color).strong());

                        ui.label("|");

                        let fr_color = if first_rate >= 60.0 { egui::Color32::GREEN } else if first_rate >= 30.0 { egui::Color32::YELLOW } else { egui::Color32::RED };
                        ui.label(egui::RichText::new(format!(
                            "一次过 {}/{} ({:.0}%)",
                            cs.first_try_passes, cs.total_attempts, first_rate
                        )).color(fr_color));

                        if cs.total_queries > 0 {
                            ui.label("|");
                            ui.label(format!("查询: {}", cs.total_queries));
                        }
                    });
                }
            }
        });
    }

    /// 浏览器实时状态面板（独立方法，两个场景共用）
    fn ui_browser_status(&mut self, ui: &mut egui::Ui) {
        if self.displayed_browser_statuses.is_empty() {
            return;
        }

        ui.push_id("browser_status_panel", |ui| {
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🖥️ 浏览器实时状态").strong());
                // 统计活跃/空闲数
                let active = self.displayed_browser_statuses.iter()
                    .filter(|bs| bs.step != BrowserStep::Idle).count();
                let total = self.displayed_browser_statuses.len();
                ui.label(format!("({}/{} 活跃)", active, total));
            });
            ui.add_space(4.0);

            egui::Grid::new("browser_status_grid")
                .striped(true)
                .num_columns(6)
                .show(ui, |ui| {
                    ui.strong("实例"); ui.strong("姓名"); ui.strong("报名号"); ui.strong("当前步骤"); ui.strong("验证码"); ui.strong("耗时");
                    ui.end_row();
                    for bs in &self.displayed_browser_statuses {
                        // 实例 ID
                        ui.label(format!("#{}", bs.id + 1));

                        // 姓名
                        let name_color = if bs.name.is_empty() {
                            egui::Color32::GRAY
                        } else {
                            egui::Color32::WHITE
                        };
                        ui.label(egui::RichText::new(if bs.name.is_empty() { "-" } else { &bs.name }).color(name_color).strong());

                        // 报名号
                        let target_display = if bs.target.len() > 16 {
                            format!("{}...", &bs.target[..16])
                        } else if bs.target.is_empty() {
                            "-".to_string()
                        } else {
                            bs.target.clone()
                        };
                        ui.label(target_display);

                        // 当前步骤（带颜色）
                        let (r, g, b) = bs.step.color();
                        let color = egui::Color32::from_rgb(r, g, b);
                        let step_text = match &bs.step {
                            BrowserStep::Error(e) => format!("❌ {}", e),
                            _ => bs.step.label().to_string(),
                        };
                        ui.label(egui::RichText::new(step_text).color(color).strong());

                        // 验证码状态
                        if bs.captcha_max > 0 && bs.captcha_attempt > 0 {
                            let captcha_text = format!("{}/{}", bs.captcha_attempt, bs.captcha_max);
                            let captcha_color = if bs.captcha_attempt == 1 {
                                egui::Color32::GREEN
                            } else if bs.captcha_attempt <= bs.captcha_max / 2 {
                                egui::Color32::YELLOW
                            } else {
                                egui::Color32::RED
                            };
                            ui.label(egui::RichText::new(captcha_text).color(captcha_color));
                        } else {
                            ui.label("-");
                        }

                        // 耗时
                        let elapsed_sec = bs.elapsed_ms as f64 / 1000.0;
                        let elapsed_text = if bs.elapsed_ms < 1000 {
                            format!("{}ms", bs.elapsed_ms)
                        } else {
                            format!("{:.1}s", elapsed_sec)
                        };
                        let elapsed_color = if bs.elapsed_ms < 5000 {
                            egui::Color32::GREEN
                        } else if bs.elapsed_ms < 15000 {
                            egui::Color32::YELLOW
                        } else {
                            egui::Color32::RED
                        };
                        ui.label(egui::RichText::new(elapsed_text).color(elapsed_color));

                        ui.end_row();
                    }
                });
        });
    }

    fn ui_results_table(&mut self, ui: &mut egui::Ui) {
        ui.push_id("results_section", |ui| {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("📊 查询结果").strong());
            ui.add_space(4.0);

            egui::ScrollArea::vertical().id_salt("results_table").max_height(200.0).show(ui, |ui| {
                TableBuilder::new(ui).id_salt("results")
                    .striped(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::auto().resizable(true))
                    .column(Column::auto().resizable(true))
                    .column(Column::auto().resizable(true))
                    .column(Column::auto().resizable(true))
                    .column(Column::auto().resizable(true))
                    .column(Column::auto().resizable(true))
                    .header(20.0, |mut h| {
                        h.col(|ui| { ui.label("姓名"); });
                        h.col(|ui| { ui.label("高考报名号"); });
                        h.col(|ui| { ui.label("身份证号"); });
                        h.col(|ui| { ui.label("科目名称"); });
                        h.col(|ui| { ui.label("考点名称"); });
                        h.col(|ui| { ui.label("状态/错误"); });
                    })
                    .body(|body| {
                        body.rows(20.0, self.displayed_results.len(), |mut row| {
                            let i = row.index();
                            if let Some(r) = self.displayed_results.get(i) {
                                row.col(|ui| { ui.label(&r.name); });
                                row.col(|ui| { ui.label(&r.baominghao); });
                                row.col(|ui| { ui.label(&r.shenfenzheng); });
                                row.col(|ui| { ui.label(&r.kemumingcheng); });
                                row.col(|ui| { ui.label(&r.kaodianmingcheng); });
                                row.col(|ui| {
                                    match &r.status {
                                        QueryStatus::Success => {
                                            ui.label(egui::RichText::new("✅ 成功").color(egui::Color32::GREEN));
                                        }
                                        QueryStatus::Failed(e) => {
                                            ui.label(egui::RichText::new(format!("❌ {}", e)).color(egui::Color32::RED));
                                        }
                                        _ => {}
                                    }
                                });
                            }
                        });
                    });
            });

            ui.add_space(8.0);
            if ui.button("💾 保存结果到文件").clicked() {
                self.save_results();
            }
        });
    }
}

// ============================================================
// 报考号推算 UI
// ============================================================
impl GaokaoApp {
    fn ui_number_prediction(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("目标网址：");
            let mut url = self.target_url.clone();
            if ui.add_sized([ui.available_width() - 60.0, 0.0], egui::TextEdit::singleline(&mut url)).changed() {
                self.target_url = url;
                self.config_dirty = true;
            }
        });
        ui.add_space(8.0);

        // file selection
        egui::Grid::new("pred_upload_grid").num_columns(3).striped(true).show(ui, |ui| {
            ui.label("身份证和信息表格：");
            let fname = self.pred_sfz_path.as_ref()
                .and_then(|p| p.split('/').last()).unwrap_or("未选择文件");
            ui.label(fname);
            if ui.button("📁 选择文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Excel", &["xlsx", "xls"]).pick_file()
                {
                    let s = path.to_string_lossy().to_string();
                    self.pred_sfz_path = Some(s.clone());
                    self.log(format!("选择身份证表格: {}", s));
                    match parser::parse_shenfenzheng(&s) {
                        Ok(records) => {
                            self.pred_sfz_records = records;
                            self.status_message = format!(
                                "解析完成: {} 条记录",
                                self.pred_sfz_records.len()
                            );
                            self.log(&self.status_message);
                        }
                        Err(e) => {
                            self.status_message = format!("解析失败: {}", e);
                            self.log(&self.status_message);
                        }
                    }
                }
            }
            ui.end_row();

            ui.label("已知报考号表格（可选）：");
            let fname2 = self.pred_bkh_path.as_ref()
                .and_then(|p| p.split('/').last()).unwrap_or("未选择文件（可选）");
            ui.label(fname2);
            if ui.button("📁 选择文件").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Excel", &["xlsx", "xls"]).pick_file()
                {
                    let s = path.to_string_lossy().to_string();
                    self.pred_bkh_path = Some(s.clone());
                    self.log(format!("选择报考号表格: {}", s));
                    match parser::parse_baokao_hao(&s) {
                        Ok(records) => {
                            let count = records.len();
                            self.pred_known_bkh = records;
                            
                            // 自动计算扫描范围：从已知考号的最小值-余量 到 最大值+余量
                            if !self.pred_known_bkh.is_empty() {
                                let mut sorted_bkh: Vec<u64> = self.pred_known_bkh.iter()
                                    .filter_map(|r| r.baominghao.parse::<u64>().ok())
                                    .filter(|&v| v > 0)
                                    .collect();
                                sorted_bkh.sort();

                                if let (Some(&min_bkh), Some(&max_bkh)) = (sorted_bkh.first(), sorted_bkh.last()) {
                                    let count = sorted_bkh.len() as u64;
                                    // 开始考号 = 最小号 - 人数（往前留余量）
                                    let auto_start = min_bkh.saturating_sub(count);
                                    // 结束考号 = 最大号 + 人数/2（往后留少量余量）
                                    let auto_end = max_bkh + count / 2;
                                    self.pred_start_bkh = auto_start;
                                    self.pred_start_str = auto_start.to_string();
                                    self.pred_end_bkh = auto_end;
                                    self.pred_end_str = auto_end.to_string();
                                    self.log(format!(
                                        "自动设置扫描范围: {} ~ {} (已知{}条, 范围跨{}个号)",
                                        auto_start, auto_end, count, auto_end - auto_start
                                    ));
                                }
                            }
                            
                            self.status_message = format!("已加载 {} 条已知报考号", count);
                            self.log(&self.status_message);
                        }
                        Err(e) => {
                            self.status_message = format!("解析失败: {}", e);
                            self.log(&self.status_message);
                        }
                    }
                }
            }
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // year selection + parameters
        if !self.pred_sfz_records.is_empty() {
            // 统计各年份人数
            let year_counts: std::collections::HashMap<u32, usize> = {
                let mut counts = std::collections::HashMap::new();
                for r in &self.pred_sfz_records {
                    let y = r.ruxue_year.unwrap_or(0.0) as u32;
                    if y >= 2022 {
                        *counts.entry(y).or_insert(0) += 1;
                    }
                }
                counts
            };

            ui.horizontal(|ui| {
                ui.label("入学年份：");
                let years: Vec<u32> = [2023, 2024, 2025].to_vec();
                let mut selected_year = self.pred_year_filter as u32;
                egui::ComboBox::from_id_salt("year_selector")
                    .selected_text(format!("{}届 ({}人)", selected_year, year_counts.get(&selected_year).unwrap_or(&0)))
                    .show_ui(ui, |ui| {
                        for &y in &years {
                            let count = year_counts.get(&y).unwrap_or(&0);
                            let label = format!("{}届 ({}人)", y, count);
                            ui.selectable_value(&mut selected_year, y, label);
                        }
                    });
                if selected_year != self.pred_year_filter as u32 {
                    self.pred_year_filter = selected_year as f64;
                    self.config_dirty = true;
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("开始考号：");
                let response = ui.add_sized(
                    [180.0, 20.0],
                    egui::TextEdit::singleline(&mut self.pred_start_str).hint_text("如 26421126151400"),
                );
                if response.changed() {
                    self.pred_start_bkh = Self::parse_bkh_number(&self.pred_start_str);
                    self.config_dirty = true;
                }
                if self.pred_start_bkh > 0 {
                    ui.label(egui::RichText::new(format!(
                        "已设置：{}", self.pred_start_bkh
                    )).color(egui::Color32::LIGHT_BLUE));
                } else {
                    ui.label(egui::RichText::new("请输入开始考号").color(egui::Color32::YELLOW));
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("结束考号：");
                let response = ui.add_sized(
                    [180.0, 20.0],
                    egui::TextEdit::singleline(&mut self.pred_end_str).hint_text("如 26421126151500"),
                );
                if response.changed() {
                    self.pred_end_bkh = Self::parse_bkh_number(&self.pred_end_str);
                    self.config_dirty = true;
                }
                if self.pred_end_bkh > 0 {
                    ui.label(egui::RichText::new(format!(
                        "已设置：{}", self.pred_end_bkh
                    )).color(egui::Color32::LIGHT_BLUE));
                } else {
                    ui.label(egui::RichText::new("请输入结束考号").color(egui::Color32::YELLOW));
                }
                if self.pred_start_bkh > 0 && self.pred_end_bkh > 0 && self.pred_end_bkh > self.pred_start_bkh {
                    ui.label(egui::RichText::new(format!(
                        "扫描范围：{} ~ {} (共{}个号)",
                        self.pred_start_bkh, self.pred_end_bkh, self.pred_end_bkh - self.pred_start_bkh
                    )).color(egui::Color32::GREEN));
                } else if self.pred_start_bkh > 0 && self.pred_end_bkh > 0 && self.pred_end_bkh <= self.pred_start_bkh {
                    ui.label(egui::RichText::new("结束考号必须大于开始考号").color(egui::Color32::RED));
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("网格探针数：");
                ui.add(egui::Slider::new(&mut self.pred_search_margin, 3..=30).text("个"));
                ui.label("（均匀分布在扫描范围内）");
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            // params (concurrency etc.)
            ui.horizontal(|ui| {
                ui.label("并发浏览器数：");
                ui.add(egui::Slider::new(&mut self.concurrency, 1..=10).text("个"));
                ui.add_space(16.0);
                ui.label("操作速度：");
                ui.add(egui::Slider::new(&mut self.step_delay_ms, 100..=5000).text("ms/步").suffix("ms"));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("验证码重试：");
                ui.add(egui::Slider::new(&mut self.captcha_retries, 1..=10).text("次"));
                ui.add_space(16.0);
                ui.label("首次等待：");
                ui.add(egui::Slider::new(&mut self.captcha_wait_ms, 500..=10000).text("ms").suffix("ms"));
            });

            ui.add_space(8.0);

            // start / stop buttons
            ui.horizontal(|ui| {
                if self.pred_state == QueryState::Idle {
                    let can_start = self.pred_start_bkh > 0 && self.pred_end_bkh > 0 && self.pred_end_bkh > self.pred_start_bkh;
                    if can_start {
                        if ui.button(egui::RichText::new("▶ 开始推算").heading().color(egui::Color32::WHITE)).clicked() {
                            self.start_prediction(ctx);
                        }
                    } else {
                        let _ = ui.button(egui::RichText::new("▶ 请设定开始/结束考号").heading().color(egui::Color32::GRAY));
                    }
                }
                if self.pred_state == QueryState::Running {
                    if ui.button(egui::RichText::new("⏹ 停止").heading()).clicked() {
                        *self.cancel_flag.try_lock().unwrap() = true;
                        self.pred_state = QueryState::Idle;
                        self.log("推算已停止");
                    }
                }
            });

            // progress
            ui.push_id("pred_progress", |ui| {
                if let Ok(p) = self.pred_progress.try_lock() {
                    if p.total > 0 {
                        ui.add_space(8.0);
                        let completed = p.matched + p.not_found;
                        let ratio = if p.total > 0 { completed as f32 / p.total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(ratio).text(format!("{}/{}", completed, p.total)));
                        ui.label(format!(
                            "✅ {}  ❌ {}  📌 {}",
                            p.matched, p.not_found, p.current_name
                        ));
                        if !p.current_batch.is_empty() {
                            ui.label(egui::RichText::new(&p.current_batch).color(egui::Color32::LIGHT_BLUE));
                        }
                    }
                }
            });

            // 验证码统计（推算场景也需要看到）
            self.ui_captcha_stats(ui);

            // 浏览器实时状态（推算场景也需要看到）
            self.ui_browser_status(ui);

            // status
            if !self.status_message.is_empty() {
                ui.add_space(4.0);
                ui.label(&self.status_message);
            }

            // results table — always show during running for real-time status
            let show_table = !self.pred_displayed_results.is_empty()
                || self.pred_state == QueryState::Running;
            if show_table {
                ui.push_id("pred_results_section", |ui| {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(format!(
                        "📊 推算结果（共 {} 条）", self.pred_displayed_results.len()
                    )).strong());
                    ui.add_space(4.0);

                    // Only redraw when data changes (perf optimization)
                    egui::ScrollArea::vertical().id_salt("pred_results_table").max_height(250.0).show(ui, |ui| {
                        TableBuilder::new(ui).id_salt("pred_results")
                            .striped(true)
                            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                            .column(Column::auto().resizable(true))
                            .column(Column::auto().resizable(true))
                            .column(Column::auto().resizable(true))
                            .column(Column::auto().resizable(true))
                            .header(20.0, |mut h| {
                                h.col(|ui| { ui.label("姓名"); });
                                h.col(|ui| { ui.label("身份证号"); });
                                h.col(|ui| { ui.label("推算报考号"); });
                                h.col(|ui| { ui.label("状态"); });
                            })
                            .body(|body| {
                                let n = self.pred_displayed_results.len();
                                body.rows(20.0, n, |mut row| {
                                    let i = row.index();
                                    if let Some(r) = self.pred_displayed_results.get(i) {
                                        row.col(|ui| { ui.label(&r.name); });
                                        row.col(|ui| { ui.label(&r.shenfenzheng); });
                                        row.col(|ui| { ui.label(&r.exam_number); });
                                        row.col(|ui| {
                                            match &r.status {
                                                PredictedStatus::Matched => {
                                                    ui.label(egui::RichText::new("✅ 已匹配").color(egui::Color32::GREEN));
                                                }
                                                PredictedStatus::NotFound => {
                                                    ui.label(egui::RichText::new("❌ 未找到").color(egui::Color32::RED));
                                                }
                                            }
                                        });
                                    }
                                });
                            });
                    });

                    ui.add_space(8.0);
                    if ui.button("💾 保存推算结果到文件").clicked() {
                        self.save_prediction_results();
                    }
                });
            }
        } else {
            ui.label("请先选择身份证和信息表格");
        }
    }
}

// ============================================================
// 考场查询 逻辑
// ============================================================
impl GaokaoApp {
    fn parse_and_match(&mut self) {
        let bk_path = match &self.baokao_path { Some(p) => p.clone(), None => return };
        let sfz_path = match &self.sfz_path { Some(p) => p.clone(), None => return };

        match parser::parse_baokao_hao(&bk_path) {
            Ok(baokao) => match parser::parse_shenfenzheng(&sfz_path) {
                Ok(sfz) => {
                    self.matched_records = matcher::match_records(&baokao, &sfz);
                    self.status_message = format!(
                        "解析完成: 报名号 {} 条, 身份证信息 {} 条, 匹配 {} 条",
                        baokao.len(), sfz.len(), self.matched_records.len()
                    );
                    self.log(&self.status_message);
                }
                Err(e) => {
                    self.status_message = format!("解析身份证表格失败: {}", e);
                    self.log(&self.status_message);
                }
            },
            Err(e) => {
                self.status_message = format!("解析报考号表格失败: {}", e);
                self.log(&self.status_message);
            }
        }
    }

    fn start_query(&mut self, _ctx: &egui::Context) {
        self.query_state = QueryState::Running;
        self.displayed_results.clear();
        *self.results.try_lock().unwrap() = Vec::new();
        *self.cancel_flag.try_lock().unwrap() = false;

        let mut seen = std::collections::HashSet::new();
        let matched: Vec<_> = self.matched_records.iter()
            .filter(|r| seen.insert((r.name.clone(), r.baominghao.clone())))
            .cloned()
            .collect();
        let concurrency = self.concurrency as usize;
        let delay = self.delay_ms as u64;
        let step_delay = self.step_delay_ms as u64;
        let captcha_retries = self.captcha_retries;
        let captcha_wait_ms = self.captcha_wait_ms as u64;
        let progress = self.progress.clone();
        let results = self.results.clone();
        let cancel = self.cancel_flag.clone();
        let hide_browser = self.hide_browser;
        let target_url = self.target_url.clone();
        let logs = self.debug_logs.clone();
        let perf_logs = self.perf_logs.clone();
        let captcha_stats = self.captcha_stats.clone();
        let browser_statuses = self.browser_statuses.clone();
        let turbo = self.config.turbo;

        {
            let mut p = progress.try_lock().unwrap();
            p.total = matched.len();
            p.completed = 0; p.success = 0; p.failed = 0;
        }

        self.log(format!("开始查询: {} 条记录, 并发 {}, 间隔 {}ms, turbo={}", matched.len(), concurrency, delay, turbo));

        tokio::spawn(async move {
            let turbo = turbo;
            let pool = match crate::browser::BrowserPool::new(
                concurrency, hide_browser, step_delay, captcha_retries, captcha_wait_ms,
                &target_url, Some(logs.clone()), turbo,
            ).await {
                Ok(p) => p,
                Err(e) => {
                    let mut l = logs.lock().await;
                    l.push(format!("[ERROR] 浏览器池启动失败: {}", e));
                    return;
                }
            };

            // 初始化浏览器状态追踪
            {
                let mut statuses = browser_statuses.lock().await;
                *statuses = (0..concurrency).map(|i| BrowserStatus {
                    id: i as u64,
                    step: BrowserStep::Idle,
                    target: String::new(),
                    name: String::new(),
                    captcha_attempt: 0,
                    captcha_max: 0,
                    elapsed_ms: 0,
                }).collect();
            }

            // 重置验证码统计
            {
                let mut cs = captcha_stats.lock().await;
                *cs = CaptchaStats::default();
            }

            let perf_logs = perf_logs.clone();
            let captcha_stats = captcha_stats.clone();
            let browser_statuses = browser_statuses.clone();
            let mut handles = Vec::new();
            for record in &matched {
                if *cancel.lock().await { break; }

                let pool = pool.clone();
                let results = results.clone();
                let progress = progress.clone();
                let cancel = cancel.clone();
                let logs = logs.clone();
                let perf_logs = perf_logs.clone();
                let captcha_stats = captcha_stats.clone();
                let browser_statuses = browser_statuses.clone();
                let record = record.clone();
                let delay = delay;

                handles.push(tokio::spawn(async move {
                    if *cancel.lock().await { return; }

                    let mut l = logs.lock().await;
                    l.push(format!("[QUERY] 开始处理: {} {}", record.name, record.baominghao));
                    drop(l);

                    {
                        let mut p = progress.lock().await;
                        p.current_name = record.name.clone();
                    }

                    let candidates = if record.shenfenzheng_candidates.is_empty() {
                        vec![String::new()]
                    } else {
                        record.shenfenzheng_candidates.clone()
                    };

                    let mut ok = false;
                    let mut last_err = String::new();

                    for (ci, sfz) in candidates.iter().enumerate() {
                        if *cancel.lock().await { break; }

                        {
                            let mut l = logs.lock().await;
                            l.push(format!("[QUERY] {} 获取浏览器中...", record.name));
                        }

                        let (permit, mut client) = pool.acquire().await;

                        let record_perf: Arc<Mutex<Vec<PerfEvent>>> = Arc::new(Mutex::new(Vec::new()));
                        client.set_perf(Some(record_perf.clone()));
                        client.set_turbo(turbo);
                        client.set_captcha_stats(Some(captcha_stats.clone()));
                        client.set_status(Some(browser_statuses.clone()));

                        {
                            let mut l = logs.lock().await;
                            l.push(format!("[QUERY] {} 浏览器已获取，开始查询", record.name));
                        }

                        if ci > 0 {
                            let _ = client.go_home().await;
                        }

                        let result = client.query_single(&record.baominghao, sfz, &record.name).await;

                        // Collect perf data
                        if let Ok(perf_data) = record_perf.try_lock() {
                            if !perf_data.is_empty() {
                                let mut pl = perf_logs.lock().await;
                                pl.push(perf_data.clone());
                            }
                        }

                        pool.release(permit, client);

                        // Track total queries (captcha attempts already tracked inside solve_captcha_modal)
                        {
                            let mut cs = captcha_stats.lock().await;
                            cs.total_queries += 1;
                        }

                        match result {
                            Ok(mut r) => {
                                r.shenfenzheng = sfz.clone();
                                let mut r_lock = results.lock().await;
                                r_lock.push(r);
                                ok = true;
                                break;
                            }
                            Err(e) => {
                                last_err = format!("候选{}失败: {}", ci + 1, e);
                                continue;
                            }
                        }
                    }

                    if !ok {
                        let mut r_lock = results.lock().await;
                        r_lock.push(QueryResult {
                            name: record.name.clone(),
                            baominghao: record.baominghao.clone(),
                            shenfenzheng: String::new(),
                            kemumingcheng: String::new(),
                            kaodianmingcheng: String::new(),
                            status: QueryStatus::Failed(last_err.clone()),
                            error: last_err.clone(),
                        });
                    }

                    {
                        let mut p = progress.lock().await;
                        p.completed += 1;
                        if ok { p.success += 1; } else { p.failed += 1; }
                    }

                    if !turbo && delay > 0 && !*cancel.lock().await {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                }));
            }
        });
    }

    fn save_results(&self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Excel", &["xlsx"])
            .set_file_name("查询结果.xlsx")
            .save_file()
        {
            use rust_xlsxwriter::*;
            let mut workbook = Workbook::new();
            let sheet = workbook.add_worksheet();
            let headers = ["姓名", "高考报名号", "身份证号", "科目名称", "考点名称", "状态/错误"];
            for (col, h) in headers.iter().enumerate() {
                let _ = sheet.write_string(0, col as u16, *h);
            }
            for (row, r) in self.displayed_results.iter().enumerate() {
                let ri = row as u32 + 1;
                let _ = sheet.write_string(ri, 0, &r.name);
                let _ = sheet.write_string(ri, 1, &r.baominghao);
                let _ = sheet.write_string(ri, 2, &r.shenfenzheng);
                let _ = sheet.write_string(ri, 3, &r.kemumingcheng);
                let _ = sheet.write_string(ri, 4, &r.kaodianmingcheng);
                let status = match &r.status {
                    QueryStatus::Success => "成功".to_string(),
                    QueryStatus::Failed(e) => format!("失败: {}", e),
                    _ => String::new(),
                };
                let _ = sheet.write_string(ri, 5, &status);
            }
            let _ = workbook.save(&path);
        }
    }
}

// ============================================================
// 报考号推算 逻辑
// ============================================================
impl GaokaoApp {
    fn start_prediction(&mut self, _ctx: &egui::Context) {
        if self.pred_start_bkh == 0 || self.pred_end_bkh == 0 || self.pred_end_bkh <= self.pred_start_bkh {
            self.status_message = "请设定有效的开始考号和结束考号（结束 > 开始）".to_string();
            return;
        }

        self.pred_state = QueryState::Running;
        *self.pred_results.try_lock().unwrap() = Vec::new();
        self.pred_displayed_results.clear();
        *self.cancel_flag.try_lock().unwrap() = false;

        let concurrency = self.concurrency as usize;
        let hide_browser = self.hide_browser;
        let step_delay = self.step_delay_ms as u64;
        let captcha_retries = self.captcha_retries;
        let captcha_wait_ms = self.captcha_wait_ms as u64;
        let target_url = self.target_url.clone();
        let turbo = self.config.turbo;
        let logs = self.debug_logs.clone();
        let perf_logs = self.perf_logs.clone();
        let cancel = self.cancel_flag.clone();
        let pred_results = self.pred_results.clone();
        let pred_progress = self.pred_progress.clone();
        let captcha_stats = self.captcha_stats.clone();
        let browser_statuses = self.browser_statuses.clone();

        // 全年级所有学生
        let all_sfz_records = self.pred_sfz_records.clone();
        let target_year = self.pred_year_filter as u32;
        let scan_low = self.pred_start_bkh;   // 开始考号（完整14位数字）
        let scan_high = self.pred_end_bkh;     // 结束考号（完整14位数字）
        let probe_count = self.pred_search_margin.max(3);

        tokio::spawn(async move {
            let pool = match crate::browser::BrowserPool::new(
                concurrency, hide_browser, step_delay, captcha_retries, captcha_wait_ms,
                &target_url, Some(logs.clone()), turbo,
            ).await {
                Ok(p) => p,
                Err(e) => {
                    let mut l = logs.lock().await;
                    l.push(format!("[ERROR] 浏览器池启动失败：{}", e));
                    return;
                }
            };

            // 初始化浏览器状态追踪（推算场景）
            {
                let mut statuses = browser_statuses.lock().await;
                *statuses = (0..concurrency).map(|i| BrowserStatus {
                    id: i as u64,
                    step: BrowserStep::Idle,
                    target: String::new(),
                    name: String::new(),
                    captcha_attempt: 0,
                    captcha_max: 0,
                    elapsed_ms: 0,
                }).collect();
            }

            // 重置验证码统计
            {
                let mut cs = captcha_stats.lock().await;
                *cs = CaptchaStats::default();
            }

            // 取全年级（指定入学年份）的所有学生
            let students: Vec<(String, String)> = all_sfz_records.iter()
                .filter(|r| {
                    let ruxue = r.ruxue_year.unwrap_or(0.0) as u32;
                    ruxue == target_year
                })
                .map(|r| (r.name.clone(), r.shenfenzheng.clone()))
                .collect();

            if students.is_empty() {
                let mut l = logs.lock().await;
                l.push(format!("[警告] 入学年份 {} 无匹配学生，终止", target_year));
                return;
            }

            let mut l = logs.lock().await;
            l.push(format!("=================================================="));
            l.push(format!("[考号推算] 启动！入学年份={} | 学生数={} | 扫描范围=[{},{}] (共{}个号)", target_year, students.len(), scan_low, scan_high, scan_high - scan_low));
            drop(l);

            // 调用推算算法（使用完整数字范围）
            let results = crate::prediction::run_prediction(
                pool,
                students,
                scan_low,
                scan_high,
                probe_count,
                concurrency,
                cancel,
                pred_progress,
                logs.clone(),
                perf_logs,
                captcha_stats,
                browser_statuses,
            ).await;

            // 写入结果
            {
                let mut r_lock = pred_results.lock().await;
                *r_lock = results;
            }

            let mut l = logs.lock().await;
            l.push(format!("🏁 所有的自动化可预测盲推流程全部收工。"));
        });
    }

    fn save_prediction_results(&self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Excel", &["xlsx"])
            .set_file_name("推算结果.xlsx")
            .save_file()
        {
            use rust_xlsxwriter::*;
            let mut workbook = Workbook::new();
            let sheet = workbook.add_worksheet();
            let headers = ["姓名", "身份证号", "推算报考号", "状态"];
            for (col, h) in headers.iter().enumerate() {
                let _ = sheet.write_string(0, col as u16, *h);
            }
            for (row, r) in self.pred_displayed_results.iter().enumerate() {
                let ri = row as u32 + 1;
                let _ = sheet.write_string(ri, 0, &r.name);
                let _ = sheet.write_string(ri, 1, &r.shenfenzheng);
                let _ = sheet.write_string(ri, 2, &r.exam_number);
                let status = match &r.status {
                    PredictedStatus::Matched => "已匹配",
                    PredictedStatus::NotFound => "未找到",
                };
                let _ = sheet.write_string(ri, 3, status);
            }
            let _ = workbook.save(&path);
        }
    }
}
