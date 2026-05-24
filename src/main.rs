mod app;
mod config;
mod data;
mod parser;
mod matcher;
mod browser;
mod ocr;

use eframe::egui;
use egui::FontDefinitions;

fn find_cjk_font() -> Option<&'static str> {
    let candidates = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/arphic/uming.ttc",
    ];
    for &p in candidates {
        if std::path::Path::new(p).exists() {
            return Some(p);
        }
    }
    None
}

fn configure_fonts(cc: &eframe::CreationContext) {
    let mut fonts = FontDefinitions::default();

    if let Some(font_path) = find_cjk_font() {
        match std::fs::read(font_path) {
            Ok(font_data) => {
                let name = "cjk".to_owned();
                fonts.font_data.insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(font_data)));

                for prop in &mut fonts.families.values_mut() {
                    prop.insert(0, name.clone());
                }

                if let Some(proportional) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    proportional.insert(0, name.clone());
                }
                if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    mono.insert(0, name);
                }
            }
            Err(e) => {
                eprintln!("加载字体失败: {}", e);
            }
        }
    }

    cc.egui_ctx.set_fonts(fonts);
}

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
        Box::new(|cc| {
            configure_fonts(cc);
            Ok(Box::new(app::GaokaoApp::default()))
        }),
    )
}
