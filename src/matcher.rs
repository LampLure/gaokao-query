use std::collections::HashMap;
use crate::data::*;

pub fn match_records(
    baokao_list: &[BaoKaoHaoRecord],
    sfz_list: &[ShenFenZhengRecord],
) -> Vec<MatchedRecord> {
    let mut name_to_sfz: HashMap<String, Vec<String>> = HashMap::new();
    for sfz in sfz_list {
        name_to_sfz.entry(sfz.name.clone())
            .or_default()
            .push(sfz.shenfenzheng.clone());
    }

    let mut matched = Vec::new();

    for bk in baokao_list {
        let candidates = name_to_sfz.get(&bk.name).cloned().unwrap_or_default();

        let status = if candidates.is_empty() {
            MatchStatus::NotFound
        } else if candidates.len() == 1 {
            MatchStatus::Matched(candidates[0].clone())
        } else {
            MatchStatus::Multiple
        };

        matched.push(MatchedRecord {
            name: bk.name.clone(),
            baominghao: bk.baominghao.clone(),
            shenfenzheng_candidates: candidates,
            baokao_info: format!("{} {} {}", bk.yuzhong, bk.kouyu, bk.leibie),
            status,
        });
    }

    matched
}
