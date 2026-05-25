use std::sync::Arc;
use tokio::sync::Mutex;
use crate::browser::BrowserPool;
use crate::data::{PredictedRecord, PredictedStatus, PredictionProgress, PerfEvent, BrowserStatus, CaptchaStats};

/// ====================================================================
/// 【锚点网格 + 密集扫射】考号推算算法
///
/// 适用于多学校考号交叉排列的场景：
///   阶段一（网格探测）：在 [0, anchor] 范围内均匀撒 probe_count 个探针点，
///       每个探针点用所有未匹配学生的身份证尝试撞击，记录命中。
///   阶段二（密集扫射）：在命中点之间及边界外扩区域逐号扫射，
///       每个号码逐个尝试剩余未匹配学生，命中即移出。
/// ====================================================================
pub async fn run_prediction(
    pool: Arc<BrowserPool>,
    students: Vec<(String, String)>,    // 全年级所有学生 (name, sfz)
    base_bkh: &str,                      // 考号前缀，如 "2642112615"
    anchor: u64,                         // 锚点后缀，如 1493
    probe_count: u32,                    // 网格探针数量（默认 10）
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
    captcha_stats: Arc<Mutex<CaptchaStats>>,
    browser_statuses: Arc<Mutex<Vec<BrowserStatus>>>,
) -> Vec<PredictedRecord> {
    let total_students = students.len();
    if total_students == 0 || anchor == 0 {
        return Vec::new();
    }

    let probe_count = probe_count.max(2) as usize;

    // 共享状态：活跃（未匹配）学生列表
    let active_students: Arc<Mutex<Vec<(String, String)>>> =
        Arc::new(Mutex::new(students.clone()));

    // 已匹配记录
    let resolved_records: Arc<Mutex<Vec<PredictedRecord>>> =
        Arc::new(Mutex::new(Vec::new()));

    // 初始化进度
    {
        let mut p = progress.lock().await;
        p.total = total_students;
        p.matched = 0;
        p.not_found = 0;
    }

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "🚀 [锚点网格] 启动！前缀={}, 锚点={}, 探针数={}, 学生数={}",
            base_bkh, anchor, probe_count, total_students
        ));
    }

    // =================================================================
    // 阶段一：网格探测（Grid Probe）
    // =================================================================
    // 生成均匀间隔的探针后缀：anchor, anchor-step, anchor-2*step, ..., 0
    let step = anchor / (probe_count as u64 - 1).max(1);
    let probe_suffixes: Vec<u64> = (0..probe_count)
        .map(|i| anchor.saturating_sub(step * i as u64))
        .chain(std::iter::once(0u64))  // 确保包含 0
        .filter(|&v| v <= anchor)
        .collect::<Vec<_>>();

    // 去重并排序（从大到小）
    let mut probe_suffixes = probe_suffixes;
    probe_suffixes.sort_unstable_by(|a, b| b.cmp(a));
    probe_suffixes.dedup();

    let total_probes = probe_suffixes.len();

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "📡 [网格探测] 生成 {} 个探针点，步长={}，范围 [0, {}]",
            total_probes, step, anchor
        ));
    }

    // 构建探针任务队列：(probe_suffix, student_name, student_sfz)
    let mut probe_tasks: Vec<(u64, String, String)> = Vec::new();
    for &suffix in &probe_suffixes {
        for (name, sfz) in &students {
            probe_tasks.push((suffix, name.clone(), sfz.clone()));
        }
    }
    // 探针阶段按后缀分组，大的先来
    probe_tasks.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    let probe_queue: Arc<Mutex<Vec<(u64, String, String)>>> =
        Arc::new(Mutex::new(probe_tasks));

    // 记录命中点：(suffix, student_name, student_sfz)
    let hit_points: Arc<Mutex<Vec<(u64, String, String)>>> =
        Arc::new(Mutex::new(Vec::new()));

    let mut probe_handles = Vec::new();

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let probe_queue = probe_queue.clone();
        let active_students = active_students.clone();
        let hit_points = hit_points.clone();
        let cancel_flag = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let base_bkh = base_bkh.to_string();
        let captcha_stats = captcha_stats.clone();
        let browser_statuses = browser_statuses.clone();

        probe_handles.push(tokio::spawn(async move {
            loop {
                if *cancel_flag.lock().await { break; }

                // 领取任务
                let task = {
                    let mut q = probe_queue.lock().await;
                    q.pop()
                };
                let (probe_suffix, name, sfz) = match task {
                    Some(t) => t,
                    None => break,
                };

                // 检查该学生是否已匹配
                {
                    let active = active_students.lock().await;
                    if !active.iter().any(|(_, s)| s == &sfz) { continue; }
                }

                // 更新进度
                {
                    let mut p = progress.lock().await;
                    p.current_batch = format!(
                        "[网格探测] 探针后缀 {} | 学生 {}",
                        probe_suffix, name
                    );
                    p.current_name = name.clone();
                    let full = format!("{}{}", base_bkh, probe_suffix);
                    p.current_exam = full;
                }

                let full_exam_number = format!("{}{}", base_bkh, probe_suffix);

                let (permit, mut client) = pool.acquire().await;
                client.set_captcha_stats(Some(captcha_stats.clone()));
                client.set_status(Some(browser_statuses.clone()));
                client.set_turbo(true);
                let record_perf = Arc::new(Mutex::new(Vec::new()));
                client.set_perf(Some(record_perf.clone()));

                let result = client.query_single(&full_exam_number, &sfz, &name).await;

                // 收集性能数据
                if let Ok(perf_data) = record_perf.try_lock() {
                    if !perf_data.is_empty() {
                        let mut pl = perf_logs.lock().await;
                        pl.push(perf_data.clone());
                    }
                }

                pool.release(permit, client);

                if let Ok(res) = result {
                    if res.name == name {
                        // 命中！从活跃池移除
                        {
                            let mut active = active_students.lock().await;
                            active.retain(|(_, s)| s != &sfz);
                        }

                        // 记录命中点
                        {
                            let mut hp = hit_points.lock().await;
                            hp.push((probe_suffix, name.clone(), sfz.clone()));
                        }

                        // 写入匹配记录
                        {
                            let mut r_lock = resolved_records.lock().await;
                            r_lock.push(PredictedRecord {
                                name: name.clone(),
                                shenfenzheng: sfz.clone(),
                                exam_number: full_exam_number.clone(),
                                status: PredictedStatus::Matched,
                            });
                        }

                        // 更新进度
                        {
                            let mut p = progress.lock().await;
                            p.matched += 1;
                        }

                        let mut l = logs.lock().await;
                        l.push(format!(
                            "🎯 [网格命中] 工人#{} 探针 {} 命中！学生：{} -> 考号：{}",
                            worker_id, probe_suffix, name, full_exam_number
                        ));
                    }
                }
            }
        }));
    }

    // 等待所有探针完成
    for h in probe_handles { let _ = h.await; }

    let matched_in_probe = {
        let p = progress.lock().await;
        p.matched
    };

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "📡 [网格探测完成] 共命中 {} / {} 人",
            matched_in_probe, total_students
        ));
    }

    // =================================================================
    // 阶段二：密集扫射（Dense Sweep）
    // =================================================================
    let remaining_count = {
        let active = active_students.lock().await;
        active.len()
    };

    if remaining_count == 0 {
        {
            let mut l = logs.lock().await;
            l.push("✅ [网格探测] 已全部命中，无需密集扫射".to_string());
        }
        return resolved_records.lock().await.clone();
    }

    // 根据命中点确定扫射范围
    let hits = hit_points.lock().await;
    let mut sweep_ranges: Vec<(u64, u64)> = Vec::new();

    if !hits.is_empty() {
        // 收集命中后缀并排序
        let mut hit_suffixes: Vec<u64> = hits.iter().map(|(s, _, _)| *s).collect();
        hit_suffixes.sort_unstable();
        hit_suffixes.dedup();

        // 计算每个命中点之间的扫射区域
        let margin = 20u64; // 边界外扩

        for i in 0..hit_suffixes.len() {
            let low = if i == 0 {
                hit_suffixes[i].saturating_sub(margin)
            } else {
                // 两个命中点之间取中点
                let prev = hit_suffixes[i - 1];
                let gap = hit_suffixes[i] - prev;
                prev + gap / 2
            };
            let high = if i == hit_suffixes.len() - 1 {
                hit_suffixes[i] + margin
            } else {
                let next = hit_suffixes[i + 1];
                let gap = next - hit_suffixes[i];
                hit_suffixes[i] + gap / 2
            };
            sweep_ranges.push((low, high));
        }

        // 合并重叠范围
        sweep_ranges.sort_unstable_by_key(|r| r.0);
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for range in sweep_ranges {
            if let Some(last) = merged.last_mut() {
                if range.0 <= last.1 + 1 {
                    last.1 = last.1.max(range.1);
                    continue;
                }
            }
            merged.push(range);
        }
        sweep_ranges = merged;
    } else {
        // 没有命中点：全范围扫射（太大了，用大步长）
        // 退回到探测范围的缩小版
        sweep_ranges.push((anchor.saturating_sub(200), anchor));
    }

    // 生成扫射号池：从大到小遍历
    let mut sweep_numbers: Vec<u64> = Vec::new();
    for (low, high) in &sweep_ranges {
        let lo = (*low).min(anchor); // 不超过锚点
        let hi = (*high).min(anchor);
        for n in (lo..=hi).rev() {
            sweep_numbers.push(n);
        }
    }

    // 去重（探针命中的号码无需再扫）
    let hit_suffix_set: std::collections::HashSet<u64> =
        hits.iter().map(|(s, _, _)| *s).collect();
    sweep_numbers.retain(|n| !hit_suffix_set.contains(n));
    sweep_numbers.sort_unstable_by(|a, b| b.cmp(a));
    sweep_numbers.dedup();

    drop(hits); // 释放 hit_points 锁

    let total_sweep = sweep_numbers.len();

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "🔫 [密集扫射] 启动！剩余 {} 人待匹配，扫射范围：{:?}（共 {} 个号码）",
            remaining_count, sweep_ranges, total_sweep
        ));
    }

    let sweep_queue: Arc<Mutex<Vec<u64>>> =
        Arc::new(Mutex::new(sweep_numbers));

    let mut sweep_handles = Vec::new();

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let sweep_queue = sweep_queue.clone();
        let active_students = active_students.clone();
        let resolved_records = resolved_records.clone();
        let cancel_flag = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let base_bkh = base_bkh.to_string();
        let captcha_stats = captcha_stats.clone();
        let browser_statuses = browser_statuses.clone();

        sweep_handles.push(tokio::spawn(async move {
            loop {
                if *cancel_flag.lock().await { break; }

                // 领取扫射号码
                let current_suffix = {
                    let mut q = sweep_queue.lock().await;
                    q.pop()
                };
                let current_suffix = match current_suffix {
                    Some(n) => n,
                    None => break,
                };

                // 检查是否还有未匹配学生
                let current_batch_students = {
                    let s_lock = active_students.lock().await;
                    s_lock.clone()
                };
                if current_batch_students.is_empty() { break; }

                let remaining = current_batch_students.len();
                let full_exam_number = format!("{}{}", base_bkh, current_suffix);

                // 更新进度
                {
                    let mut p = progress.lock().await;
                    p.current_batch = format!(
                        "[密集扫射] 考号后缀 {} (剩余{}人待匹配)",
                        current_suffix, remaining
                    );
                    p.current_exam = full_exam_number.clone();
                }

                // 用这个号码撞击所有剩余未匹配学生
                for (name, sfz) in &current_batch_students {
                    if *cancel_flag.lock().await { break; }

                    // 再次检查该学生是否已被其他工人匹配
                    {
                        let active = active_students.lock().await;
                        if !active.iter().any(|(_, s)| s == &sfz) { continue; }
                    }

                    let (permit, mut client) = pool.acquire().await;
                    client.set_captcha_stats(Some(captcha_stats.clone()));
                    client.set_status(Some(browser_statuses.clone()));
                    client.set_turbo(true);
                    let record_perf = Arc::new(Mutex::new(Vec::new()));
                    client.set_perf(Some(record_perf.clone()));

                    let result = client.query_single(&full_exam_number, &sfz, &name).await;

                    // 收集性能数据
                    if let Ok(perf_data) = record_perf.try_lock() {
                        if !perf_data.is_empty() {
                            let mut pl = perf_logs.lock().await;
                            pl.push(perf_data.clone());
                        }
                    }

                    pool.release(permit, client);

                    if let Ok(res) = result {
                        if res.name == *name {
                            // 命中！从活跃池移除
                            {
                                let mut active = active_students.lock().await;
                                active.retain(|(_, s)| s != sfz);
                            }

                            // 写入匹配记录
                            {
                                let mut r_lock = resolved_records.lock().await;
                                r_lock.push(PredictedRecord {
                                    name: name.clone(),
                                    shenfenzheng: sfz.clone(),
                                    exam_number: full_exam_number.clone(),
                                    status: PredictedStatus::Matched,
                                });
                            }

                            // 更新进度
                            {
                                let mut p = progress.lock().await;
                                p.matched += 1;
                                p.current_name = name.clone();
                            }

                            let mut l = logs.lock().await;
                            l.push(format!(
                                "✨ [扫射命中] 工人#{} 考号后缀 {} 命中！学生：{} -> 考号：{}",
                                worker_id, current_suffix, name, full_exam_number
                            ));

                            // 一个号码最多匹配一个学生，命中后立即换下一个号码
                            break;
                        }
                    }
                }
            }
        }));
    }

    // 等待所有扫射完成
    for h in sweep_handles { let _ = h.await; }

    // =================================================================
    // 收尾：未匹配的学生标记为 NotFound
    // =================================================================
    let final_active = active_students.lock().await;
    let mut out_records = resolved_records.lock().await.clone();

    for (name, sfz) in final_active.iter() {
        out_records.push(PredictedRecord {
            name: name.clone(),
            shenfenzheng: sfz.clone(),
            exam_number: "扫描范围外或转学".to_string(),
            status: PredictedStatus::NotFound,
        });
        {
            let mut p = progress.lock().await;
            p.not_found += 1;
        }
    }

    let final_matched = {
        let p = progress.lock().await;
        p.matched
    };

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "🏁 [推算完成] 最终结果：命中 {} / {}，未找到 {}",
            final_matched, total_students, final_active.len()
        ));
    }

    out_records
}
