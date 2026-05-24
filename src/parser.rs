use calamine::{open_workbook, DataType, Reader, Xlsx};
use crate::data::{BaoKaoHaoRecord, ShenFenZhengRecord};

fn cell_str<'a>(cell: &'a calamine::Data) -> Option<&'a str> {
    cell.get_string()
}

fn cell_float_str(cell: &calamine::Data) -> Option<String> {
    cell.get_float().map(|v| format!("{:.0}", v))
}

fn cell_value(cell: &calamine::Data) -> String {
    match cell_str(cell) {
        Some(s) => s.to_string(),
        None => cell_float_str(cell).unwrap_or_default(),
    }
}

pub fn parse_baokao_hao(path: &str) -> Result<Vec<BaoKaoHaoRecord>, String> {
    let mut workbook: Xlsx<_> = open_workbook(path).map_err(|e| format!("无法打开报考号表格: {}", e))?;

    let range = workbook.worksheet_range_at(0)
        .ok_or_else(|| "找不到工作表".to_string())?
        .map_err(|e| format!("读取工作表失败: {}", e))?;

    let mut records = Vec::new();

    for (i, row) in range.rows().enumerate() {
        if i == 0 { continue; }
        if row.len() < 5 { continue; }

        let baominghao = cell_value(row.get(1).unwrap_or(&calamine::Data::Empty));
        let name = cell_str(row.get(2).unwrap_or(&calamine::Data::Empty))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        if name.is_empty() || baominghao.is_empty() { continue; }

        records.push(BaoKaoHaoRecord {
            xuhao: row.get(0).and_then(|c| c.get_float()),
            baominghao,
            name,
            yuzhong: cell_str(row.get(3).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
            kouyu: cell_str(row.get(4).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
            leibie: cell_str(row.get(5).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
        });
    }

    Ok(records)
}

pub fn parse_shenfenzheng(path: &str) -> Result<Vec<ShenFenZhengRecord>, String> {
    let mut workbook: Xlsx<_> = open_workbook(path).map_err(|e| format!("无法打开身份证表格: {}", e))?;

    let sheet_names = workbook.sheet_names().clone();
    let mut records = Vec::new();

    for sheet_idx in 0..sheet_names.len() {
        let name = &sheet_names[sheet_idx];
        if name.contains("字段说明") || name.trim().is_empty() {
            continue;
        }

        let range = match workbook.worksheet_range_at(sheet_idx) {
            Some(Ok(r)) => r,
            _ => continue,
        };

        let mut in_data = false;

        for row in range.rows() {
            let first = cell_str(row.get(0).unwrap_or(&calamine::Data::Empty)).unwrap_or("");

            if first.contains("登录账号") || first.contains("登录") {
                in_data = true;
                continue;
            }

            if !in_data { continue; }

            let shenfenzheng = cell_value(row.get(0).unwrap_or(&calamine::Data::Empty));
            let name = cell_str(row.get(3).unwrap_or(&calamine::Data::Empty))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            if name.is_empty() || shenfenzheng.is_empty() { continue; }

            records.push(ShenFenZhengRecord {
                shenfenzheng: shenfenzheng.trim().to_string(),
                password: cell_str(row.get(1).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
                bianhao: cell_value(row.get(2).unwrap_or(&calamine::Data::Empty)),
                name,
                gender: cell_str(row.get(4).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
                birth: cell_value(row.get(5).unwrap_or(&calamine::Data::Empty)),
                organization: cell_str(row.get(6).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
                phone: cell_str(row.get(7).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                email: cell_str(row.get(9).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                ruxue_year: row.get(10).and_then(|c| c.get_float()),
                minzu: cell_str(row.get(11).unwrap_or(&calamine::Data::Empty)).unwrap_or("").to_string(),
                zhengzhi: cell_str(row.get(12).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                wenhua: cell_str(row.get(13).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                zongjiao: cell_str(row.get(14).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                hunyin: cell_str(row.get(15).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                xueji: cell_str(row.get(16).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
                zhuanye: cell_str(row.get(17).unwrap_or(&calamine::Data::Empty)).map(|s| s.to_string()),
            });
        }
    }

    Ok(records)
}
