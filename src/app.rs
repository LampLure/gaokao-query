use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::data::*;
use crate::matcher;
use crate::parser;

#[derive(PartialEq)]
enum Panel {
    Query,
}

#[derive(Debug, Clone, Default)]
pub struct AppProgress {
    pub total: usize,
    pub completed: usize,
    pub success: usize,
    pub failed: usize,
    pub current_name: String,
}

pub struct GaokaoApp {
    selected_panel: Panel,
    baokao_path: Option<String>,
    sfz_path: Option<String>,
    matched_records: Vec<MatchedRecord>,
    results: Arc<Mutex<Vec<QueryResult>>>,
    displayed_results: Vec<QueryResult>,
    concurrency: u32,
    delay_ms: u32,
    is_querying: bool,
    progress: Arc<Mutex<AppProgress>>,
    status_message: String,
}

impl Default for GaokaoApp {
    fn default() -> Self {
        Self {
            selected_panel: Panel::Query,
            baokao_path: None,
            sfz_path: None,
            matched_records: Vec::new(),
            results: Arc::new(Mutex::new(Vec::new())),
            displayed_results: Vec::new(),
            concurrency: 3,
            delay_ms: 2000,
            is_querying: false,
            progress: Arc::new(Mutex::new(AppProgress::default())),
            status_message: String::new(),
        }
    }
}

impl eframe::App for GaokaoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Sync results from async tasks to UI
        if let Ok(r) = self.results.try_lock() {
            if r.len() != self.displayed_results.len() {
                self.displayed_results = r.clone();
            }
        }

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(180.0)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    ui.label(egui::RichText::new("高考查询系统").heading().strong());
                    ui.separator();
                    ui.add_space(8.0);

                    if ui
                        .selectable_label(self.selected_panel == Panel::Query, "报名号+身份证查询")
                        .clicked()
                    {
                        self.selected_panel = Panel::Query;
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.selected_panel {
            Panel::Query => self.show_query_panel(ui, ctx),
        });
    }
}

impl GaokaoApp {
    fn show_query_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("报名号 + 身份证号 查询报考信息");
        ui.separator();
        ui.add_space(8.0);

        // File selection grid
        egui::Grid::new("upload_grid")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                ui.label("报考号表格：");
                let fname = self
                    .baokao_path
                    .as_ref()
                    .and_then(|p| p.split('/').last())
                    .unwrap_or("未选择文件");
                ui.label(fname);
                if ui.button("📁 选择文件").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Excel", &["xlsx", "xls"])
                        .pick_file()
                    {
                        self.baokao_path = Some(path.to_string_lossy().to_string());
                        self.parse_and_match();
                    }
                }
                ui.end_row();

                ui.label("身份证和信息表格：");
                let fname2 = self
                    .sfz_path
                    .as_ref()
                    .and_then(|p| p.split('/').last())
                    .unwrap_or("未选择文件");
                ui.label(fname2);
                if ui.button("📁 选择文件").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Excel", &["xlsx", "xls"])
                        .pick_file()
                    {
                        self.sfz_path = Some(path.to_string_lossy().to_string());
                        self.parse_and_match();
                    }
                }
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Matched records table
        if !self.matched_records.is_empty() {
            ui.label(
                egui::RichText::new(format!(
                    "匹配结果（共 {} 条记录）",
                    self.matched_records.len()
                ))
                .strong(),
            );
            ui.add_space(4.0);

            egui::ScrollArea::vertical()
                .max_height(200.0)
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .header(20.0, |mut header| {
                            header.col(|ui| {
                                ui.label("姓名");
                            });
                            header.col(|ui| {
                                ui.label("报名号");
                            });
                            header.col(|ui| {
                                ui.label("身份证号");
                            });
                            header.col(|ui| {
                                ui.label("报考信息");
                            });
                            header.col(|ui| {
                                ui.label("状态");
                            });
                        })
                        .body(|body| {
                            body.rows(20.0, self.matched_records.len(), |mut row| {
                                let i = row.index();
                                if let Some(record) = self.matched_records.get(i) {
                                    row.col(|ui| {
                                        ui.label(&record.name);
                                    });
                                    row.col(|ui| {
                                        ui.label(&record.baominghao);
                                    });
                                    row.col(|ui| {
                                        let sfz = match &record.status {
                                            MatchStatus::Matched(s) => s.clone(),
                                            MatchStatus::Multiple => format!(
                                                "同名{}人",
                                                record.shenfenzheng_candidates.len()
                                            ),
                                            MatchStatus::NotFound => "未找到".to_string(),
                                            MatchStatus::Pending => "待匹配".to_string(),
                                        };
                                        ui.label(sfz);
                                    });
                                    row.col(|ui| {
                                        ui.label(&record.baokao_info);
                                    });
                                    row.col(|ui| {
                                        let (text, color) = match &record.status {
                                            MatchStatus::Matched(_) => {
                                                ("已匹配", egui::Color32::GREEN)
                                            }
                                            MatchStatus::Multiple => {
                                                ("同名需穷举", egui::Color32::YELLOW)
                                            }
                                            MatchStatus::NotFound => {
                                                ("未找到", egui::Color32::RED)
                                            }
                                            MatchStatus::Pending => {
                                                ("待匹配", egui::Color32::GRAY)
                                            }
                                        };
                                        ui.label(egui::RichText::new(text).color(color));
                                    });
                                }
                            });
                        });
                });
        }

        ui.add_space(8.0);

        // Query controls
        if !self.matched_records.is_empty() && !self.is_querying {
            ui.horizontal(|ui| {
                ui.label("并发数：");
                ui.add(egui::Slider::new(&mut self.concurrency, 1..=10).text("个"));
                ui.add_space(16.0);
                ui.label("查询间隔：");
                ui.add(
                    egui::Slider::new(&mut self.delay_ms, 0..=10000)
                        .text("毫秒")
                        .suffix("ms"),
                );
            });

            ui.add_space(8.0);

            if ui
                .button(
                    egui::RichText::new("▶ 开始查询")
                        .heading()
                        .color(egui::Color32::WHITE),
                )
                .clicked()
            {
                self.start_query(ctx);
            }
        }

        // Progress
        {
            if let Ok(p) = self.progress.try_lock() {
                if p.total > 0 {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    let ratio = if p.total > 0 {
                        p.completed as f32 / p.total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(ratio)
                            .text(format!("{}/{}", p.completed, p.total)),
                    );
                    ui.label(format!(
                        "✅ 成功: {}   ❌ 失败: {}   📌 当前: {}",
                        p.success, p.failed, p.current_name
                    ));
                }
            }
        }

        // Status message
        if !self.status_message.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(&self.status_message);
        }

        // Results table
        if !self.displayed_results.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("📊 查询结果").strong());
            ui.add_space(4.0);

            egui::ScrollArea::vertical()
                .max_height(250.0)
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .column(Column::auto().resizable(true))
                        .header(20.0, |mut header| {
                            header.col(|ui| {
                                ui.label("姓名");
                            });
                            header.col(|ui| {
                                ui.label("高考报名号");
                            });
                            header.col(|ui| {
                                ui.label("身份证号");
                            });
                            header.col(|ui| {
                                ui.label("科目名称");
                            });
                            header.col(|ui| {
                                ui.label("考点名称");
                            });
                        })
                        .body(|body| {
                            body.rows(20.0, self.displayed_results.len(), |mut row| {
                                let i = row.index();
                                if let Some(r) = self.displayed_results.get(i) {
                                    row.col(|ui| {
                                        ui.label(&r.name);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.baominghao);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.shenfenzheng);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.kemumingcheng);
                                    });
                                    row.col(|ui| {
                                        ui.label(&r.kaodianmingcheng);
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

        // Auto-refresh during query
        if self.is_querying {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn parse_and_match(&mut self) {
        let bk_path = match &self.baokao_path {
            Some(p) => p.clone(),
            None => return,
        };
        let sfz_path = match &self.sfz_path {
            Some(p) => p.clone(),
            None => return,
        };

        match parser::parse_baokao_hao(&bk_path) {
            Ok(baokao) => match parser::parse_shenfenzheng(&sfz_path) {
                Ok(sfz) => {
                    self.matched_records = matcher::match_records(&baokao, &sfz);
                    self.status_message = format!(
                        "解析完成: 报名号 {} 条, 身份证信息 {} 条, 匹配 {} 条",
                        baokao.len(),
                        sfz.len(),
                        self.matched_records.len()
                    );
                }
                Err(e) => {
                    self.status_message = format!("解析身份证表格失败: {}", e);
                }
            },
            Err(e) => {
                self.status_message = format!("解析报考号表格失败: {}", e);
            }
        }
    }

    fn start_query(&mut self, _ctx: &egui::Context) {
        self.is_querying = true;
        self.displayed_results.clear();
        *self.results.try_lock().unwrap() = Vec::new();

        let matched = self.matched_records.clone();
        let concurrency = self.concurrency as usize;
        let delay = self.delay_ms as u64;
        let progress = self.progress.clone();
        let results = self.results.clone();

        {
            let mut p = progress.try_lock().unwrap();
            p.total = matched.len();
            p.completed = 0;
            p.success = 0;
            p.failed = 0;
        }

        tokio::spawn(async move {
            let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));

            for record in &matched {
                let sem = semaphore.clone();
                let progress = progress.clone();
                let results = results.clone();
                let record = record.clone();
                let delay = delay;

                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();

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
                    for sfz in &candidates {
                        match crate::browser::BrowserClient::new().await {
                            Ok(client) => {
                                match client.query_single(&record.baominghao, sfz).await {
                                    Ok(r) => {
                                        let mut r_lock = results.lock().await;
                                        r_lock.push(r);
                                        ok = true;
                                        break;
                                    }
                                    Err(_) => continue,
                                }
                            }
                            Err(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                                continue;
                            }
                        }
                    }

                    {
                        let mut p = progress.lock().await;
                        p.completed += 1;
                        if ok {
                            p.success += 1;
                        } else {
                            p.failed += 1;
                        }
                    }

                    drop(_permit);
                    if delay > 0 {
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

            let headers = ["姓名", "高考报名号", "身份证号", "科目名称", "考点名称"];
            for (col, h) in headers.iter().enumerate() {
                let _ = sheet.write_string(0, col as u16, *h);
            }

            for (row, r) in self.displayed_results.iter().enumerate() {
                let r_idx = row as u32 + 1;
                let _ = sheet.write_string(r_idx, 0, &r.name);
                let _ = sheet.write_string(r_idx, 1, &r.baominghao);
                let _ = sheet.write_string(r_idx, 2, &r.shenfenzheng);
                let _ = sheet.write_string(r_idx, 3, &r.kemumingcheng);
                let _ = sheet.write_string(r_idx, 4, &r.kaodianmingcheng);
            }

            match workbook.save(&path) {
                Ok(_) => {
                    // Status will be shown on next UI refresh
                }
                Err(e) => {
                    eprintln!("保存失败: {}", e);
                }
            }
        }
    }
}
