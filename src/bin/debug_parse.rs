use gaokao::parser;
use gaokao::matcher;

fn main() {
    // Check the actual worksheet dimensions
    use calamine::{open_workbook, Reader, Xlsx};
    let mut workbook: Xlsx<_> = open_workbook("/mnt/windows/Dpan/高考分析软件/身份证和信息表格.xlsx").unwrap();
    let n_sheets = workbook.sheet_names().len();
    println!("工作表数量: {}", n_sheets);
    for name in workbook.sheet_names() {
        println!("  工作表: {}", name);
    }

    let range = workbook.worksheet_range_at(0)
        .ok_or("no sheet")
        .unwrap()
        .map_err(|e| format!("err: {}", e))
        .unwrap();
    println!("范围: {:?} 行x列", (range.height(), range.width()));
    println!("总行数(含header): {}", range.height());

    let bk = parser::parse_baokao_hao("/mnt/windows/Dpan/高考分析软件/报考号表格.xlsx").unwrap();
    let sfz = parser::parse_shenfenzheng("/mnt/windows/Dpan/高考分析软件/身份证和信息表格.xlsx").unwrap();
    let matched = matcher::match_records(&bk, &sfz);

    println!("报考号记录数: {}", bk.len());
    println!("身份证记录数: {}", sfz.len());

    if sfz.len() > 1500 {
        for i in sfz.len().saturating_sub(5)..sfz.len() {
            println!("SFZ[{}]: name={:<10} sfz={}", i, sfz[i].name, sfz[i].shenfenzheng);
        }
    }

    println!("\n匹配记录数: {}", matched.len());
    for m in &matched {
        let status = match &m.status {
            gaokao::data::MatchStatus::Matched(s) => format!("已匹配: {}", s),
            gaokao::data::MatchStatus::Multiple => format!("同名(共{}人)", m.shenfenzheng_candidates.len()),
            gaokao::data::MatchStatus::NotFound => "未找到".to_string(),
            gaokao::data::MatchStatus::Pending => "待匹配".to_string(),
        };
        println!("{:<12} {:<20} [{}]", m.name, m.baominghao, status);
    }
}
