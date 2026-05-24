use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;
use crate::data::*;
use crate::matcher;
use crate::parser;

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
    // state
    baokao_path: Option<String>,
    sfz_path: Option<String>,
    matched_records: Vec<MatchedRecord>,
    results: Arc<Mutex<Vec<QueryResult>>>,
    displayed_results: Vec<QueryResult>,
    query_state: QueryState,
    concurrency: u32,
    delay_ms: u32,
    progress: Arc<Mutex<QueryProgress>>,
    status_message: String,
    // debug
    debug_mode: bool,
    debug_logs: Arc<Mutex<Vec<String>>>,
    displayed_logs: Vec<String>,
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
            matched_records: Vec::new(),
            results: Arc::new(Mutex::new(Vec::new())),
            displayed_results: Vec::new(),
            query_state: QueryState::Idle,
            concurrency: cfg.concurrency,
            delay_ms: cfg.delay_ms,
            progress: Arc::new(Mutex::new(QueryProgress::default())),
            status_message: String::new(),
            debug_mode: cfg.debug_mode,
            debug_logs: Arc::new(Mutex::new(Vec::new())),
            displayed_logs: Vec::new(),
            cancel_flag: Arc::new(Mutex::new(false)),
        };
        // auto-parse if saved paths exist
        if app.baokao_path.is_some() && app.sfz_path.is_some() {
            app.parse_and_match();
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
        self.config.concurrency = self.concurrency;
        self.config.delay_ms = self.delay_ms;
        self.config.debug_mode = self.debug_mode;
        config::save(&self.config);
        self.config_dirty = false;
    }
}

impl eframe::App for GaokaoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // sync async results to ui
        if let Ok(r) = self.results.try_lock() {
            if r.len() != self.displayed_results.len() {
                self.displayed_results = r.clone();
            }
        }
        if let Ok(l) = self.debug_logs.try_lock() {
            if l.len() != self.displayed_logs.len() {
                self.displayed_logs = l.clone();
            }
        }

        // auto save
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
                    ui.label("查询功能");
                    ui.label("  ▶ 报名号+身份证查询");
                    ui.separator();
                    ui.add_space(8.0);

                    // debug toggle
                    let was = self.debug_mode;
                    ui.checkbox(&mut self.debug_mode, "🔧 调试模式");
                    if was != self.debug_mode {
                        self.config_dirty = true;
                        self.log(if self.debug_mode { "调试模式已开启" } else { "调试模式已关闭" });
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
                    egui::ScrollArea::vertical().id_source("debug_logs")
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &logs {
                                ui.label(line);
                            }
                        });
                });
        }

        // === central panel ===
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("报名号 + 身份证号 查询报考信息");
            ui.separator();
            ui.add_space(8.0);

            // file selection
            egui::Grid::new("upload_grid")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
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
                ui.label(egui::RichText::new(format!(
                    "📋 匹配结果（共 {} 条）", self.matched_records.len()
                )).strong());
                ui.add_space(4.0);

                egui::ScrollArea::vertical().id_source("matched_table").max_height(180.0).show(ui, |ui| {
                    TableBuilder::new(ui).id_source("matched")
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
            }

            ui.add_space(8.0);

            // query controls
            if !self.matched_records.is_empty() && self.query_state != QueryState::Running {
                ui.horizontal(|ui| {
                    ui.label("并发数：");
                    ui.add(egui::Slider::new(&mut self.concurrency, 1..=10).text("个"));
                    ui.add_space(16.0);
                    ui.label("查询间隔：");
                    ui.add(egui::Slider::new(&mut self.delay_ms, 0..=10000).text("毫秒").suffix("ms"));
                });

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.push_id("query_controls", |ui| {
                        if self.query_state == QueryState::Idle {
                            if ui.button(egui::RichText::new("▶ 开始查询").heading().color(egui::Color32::WHITE)).clicked() {
                                self.start_query(ctx);
                            }
                        }
                        if self.query_state == QueryState::Paused {
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
                        }
                    });
                });
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

            // progress
            {
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
            }

            if !self.status_message.is_empty() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(&self.status_message);
            }

            // results table
            if !self.displayed_results.is_empty() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(egui::RichText::new("📊 查询结果").strong());
                ui.add_space(4.0);

                egui::ScrollArea::vertical().id_source("results_table").max_height(200.0).show(ui, |ui| {
                    TableBuilder::new(ui).id_source("results")
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
            }

            if self.query_state == QueryState::Running || self.query_state == QueryState::Paused {
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        });
    }
}

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

    fn start_query(&mut self, ctx: &egui::Context) {
        self.query_state = QueryState::Running;
        self.displayed_results.clear();
        *self.results.try_lock().unwrap() = Vec::new();
        *self.cancel_flag.try_lock().unwrap() = false;

        let matched = self.matched_records.clone();
        let concurrency = self.concurrency as usize;
        let delay = self.delay_ms as u64;
        let progress = self.progress.clone();
        let results = self.results.clone();
        let cancel = self.cancel_flag.clone();
        let debug_mode = self.debug_mode;
        let logs = self.debug_logs.clone();

        {
            let mut p = progress.try_lock().unwrap();
            p.total = matched.len();
            p.completed = 0; p.success = 0; p.failed = 0;
        }

        self.log(format!("开始查询: {} 条记录, 并发 {}, 间隔 {}ms, 调试={}",
            matched.len(), concurrency, delay, debug_mode));

        let cancel_check = cancel.clone();
        tokio::spawn(async move {
            let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));

            for record in &matched {
                // check cancel
                if *cancel_check.lock().await { break; }

                let sem = semaphore.clone();
                let progress = progress.clone();
                let results = results.clone();
                let record = record.clone();
                let cancel_inner = cancel_check.clone();
                let delay = delay;
                let debug_mode = debug_mode;
                let logs = logs.clone();

                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();

                    // check cancel
                    if *cancel_inner.lock().await { return; }

                    {
                        let mut p = progress.lock().await;
                        p.current_name = record.name.clone();
                        let _ = logs.lock().await;
                    }

                    let candidates = if record.shenfenzheng_candidates.is_empty() {
                        vec![String::new()]
                    } else {
                        record.shenfenzheng_candidates.clone()
                    };

                    let mut ok = false;
                    let mut last_err = String::new();
                    for sfz in &candidates {
                        if *cancel_inner.lock().await { return; }
                        match crate::browser::BrowserClient::new_with_log(debug_mode, Some(logs.clone())).await {
                            Ok(client) => {
                                match client.query_single(&record.baominghao, sfz).await {
                                    Ok(mut r) => {
                                        r.shenfenzheng = sfz.clone();
                                        let mut r_lock = results.lock().await;
                                        r_lock.push(r);
                                        ok = true;
                                        break;
                                    }
                                    Err(e) => {
                                        last_err = e;
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                last_err = e;
                                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
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
                        let _ = logs.lock().await;
                    }

                    {
                        let mut p = progress.lock().await;
                        p.completed += 1;
                        if ok { p.success += 1; } else { p.failed += 1; }
                    }

                    drop(_permit);
                    if delay > 0 && !*cancel_inner.lock().await {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                });
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
