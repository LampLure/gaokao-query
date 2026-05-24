mod app;
mod data;
mod parser;
mod matcher;
mod browser;
mod ocr;

use eframe::egui;

#[tokio::main]
async fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("高考分析查询系统"),
        ..Default::default()
    };

    eframe::run_native(
        "高考分析查询系统",
        options,
        Box::new(|_cc| Ok(Box::new(app::GaokaoApp::default()))),
    )
}
