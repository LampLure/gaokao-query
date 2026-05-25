use std::sync::Arc;
use tokio::sync::Mutex;
use crate::browser::BrowserPool;
use crate::data::{PredictedRecord, PredictedStatus, PredictionProgress, PerfEvent, BrowserStatus, CaptchaStats};

/// ====================================================================
/// 【顺序扫描】考号推算算法
///
/// 用户直接指定开始考号和结束考号，从开始到结束逐号扫描。
/// 所有参数都是完整14位考号数字（如 26421126151462）。
///
/// 算法流程：
///   1. 从开始考号到结束考号，逐个号码尝试
///   2. 每个号码用所有未匹配学生的身份证依次撞击
///   3. 命中后该学生从活跃池移除
///   4. 所有学生匹配完成或所有号码穷尽后结束
///
/// 支持多浏览器并发：多个工人同时从号码池中取号，并行扫描
/// ====================================================================
pub async fn run_prediction(
    pool: Arc<BrowserPool>,
    students: Vec<(String, String)>,    // 全年级所有学生 (name, sfz)
    scan_low: u64,                      // 开始考号（完整14位数字）
    scan_high: u64,                     // 结束考号（完整14位数字）
    _probe_count: u32,                  // 保留参数（不再使用网格探测）
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
    captcha_stats: Arc<Mutex<CaptchaStats>>,
    browser_statuses: Arc<Mutex<Vec<BrowserStatus>>>,
) -> Vec<PredictedRecord> {
    let total_students = students.len();
    if total_students == 0 || scan_low == 0 || scan_high == 0 || scan_high <= scan_low {
        return Vec::new();
    }

    let scan_range = scan_high - scan_low;

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
            "🚀 [顺序扫描] 启动！扫描范围=[{}, {}], 共{}个号, 学生数={}",
            scan_low, scan_high, scan_range, total_students
        ));
    }

    // =================================================================
    // 生成号码池：从开始考号到结束考号，顺序排列
    // =================================================================
    let scan_numbers: Vec<u64> = (scan_low..=scan_high).collect();

    let total_numbers = scan_numbers.len();

    {
        let mut l = logs.lock().await;
        l.push(format!(
            "📋 [顺序扫描] 号码池大小：{} 个号码，范围 [{}, {}]",
            total_numbers, scan_low, scan_high
        ));
        // 打印前5个和后5个号码
        let first5: Vec<String> = scan_numbers.iter().take(5).map(|n| n.to_string()).collect();
        let last5: Vec<String> = scan_numbers.iter().rev().take(5).rev().map(|n| n.to_string()).collect();
        l.push(format!("📋 [顺序扫描] 前5个: {}", first5.join(", ")));
        l.push(format!("📋 [顺序扫描] 后5个: {}", last5.join(", ")));
    }

    // 号码队列（并发安全）：工人们从这里取号
    let scan_queue: Arc<Mutex<Vec<u64>>> =
        Arc::new(Mutex::new(scan_numbers));

    let mut worker_handles = Vec::new();

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let scan_queue = scan_queue.clone();
        let active_students = active_students.clone();
        let resolved_records = resolved_records.clone();
        let cancel_flag = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let captcha_stats = captcha_stats.clone();
        let browser_statuses = browser_statuses.clone();

        worker_handles.push(tokio::spawn(async move {
            loop {
                if *cancel_flag.lock().await { break; }

                // 领取下一个号码
                let current_number = {
                    let mut q = scan_queue.lock().await;
                    q.pop()
                };
                let current_number = match current_number {
                    Some(n) => n,
                    None => break, // 号码池空了
                };

                // 检查是否还有未匹配学生
                let current_batch_students = {
                    let s_lock = active_students.lock().await;
                    s_lock.clone()
                };
                if current_batch_students.is_empty() { break; }

                let remaining = current_batch_students.len();
                let full_exam_number = current_number.to_string();

                // 更新进度
                {
                    let mut p = progress.lock().await;
                    p.current_batch = format!(
                        "[扫描] 考号 {} (剩余{}人待匹配)",
                        current_number, remaining
                    );
                    p.current_exam = full_exam_number.clone();
                }

                // 用这个号码撞击所有剩余未匹配学生
                for (name, sfz) in &current_batch_students {
                    if *cancel_flag.lock().await { break; }

                    // 再次检查该学生是否已被其他工人匹配
                    {
                        let active = active_students.lock().await;
                        if !active.iter().any(|(_, s)| s == sfz) { continue; }
                    }

                    // 更新进度：当前正在尝试的学生
                    {
                        let mut p = progress.lock().await;
                        p.current_name = name.clone();
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
                                "✅ [命中] 工人#{} 考号 {} 命中！学生：{}",
                                worker_id, current_number, name
                            ));

                            // 一个号码最多匹配一个学生，命中后立即换下一个号码
                            break;
                        }
                    }
                }
            }
        }));
    }

    // 等待所有工人完成
    for h in worker_handles { let _ = h.await; }

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
