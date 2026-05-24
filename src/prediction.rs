use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use crate::browser::BrowserPool;
use crate::data::{PredictedRecord, PredictedStatus, PredictionProgress, PerfEvent};

/// ====================================================================
/// 【核心算法阶段一】分布式雷达盲推（方案 A：自适应大步长考号广播 + 动态熔断）
/// ====================================================================
pub async fn run_radar_probes(
    pool: Arc<BrowserPool>,
    students: &Vec<(String, String)>, // 全班学生的 (姓名，身份证)
    base_bkh: &str,
    probes: Vec<(u64, u32)>,          // Vec<(探针号尾数，探针深度深度标签)>
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    logs: Arc<Mutex<Vec<String>>>,
) -> Option<(u64, u32)> { // 如果撞中，返回 (中奖号尾数，探针标签)

    // 构建雷达探测任务矩阵：3 个探针号 × 全班 N 个身份证
    let mut radar_tasks = Vec::new();
    for &(probe_bkh, depth) in &probes {
        let full_probe_string = format!("{}{}", base_bkh, probe_bkh);
        for (name, sfz) in students {
            radar_tasks.push((probe_bkh, full_probe_string.clone(), name.clone(), sfz.clone(), depth));
        }
    }

    // 内部通道，用于捕获首个成功碰撞的回波
    let (tx, mut rx) = mpsc::channel::<(u64, u32)>(10);
    let radar_cancel = Arc::new(Mutex::new(false)); // 内部批次熔断信号

    let mut worker_handles = Vec::new();
    let tasks_arc = Arc::new(Mutex::new(radar_tasks));

    for worker_id in 0..concurrency {
        let pool = pool.clone();
        let tasks = tasks_arc.clone();
        let tx = tx.clone();
        let inner_cancel = radar_cancel.clone();
        let global_cancel = cancel_flag.clone();
        let logs = logs.clone();

        worker_handles.push(tokio::spawn(async move {
            loop {
                // 检查外部中断或内部中奖熔断
                if *global_cancel.lock().await || *inner_cancel.lock().await { break; }

                // 领取探针任务
                let task = {
                    let mut t_lock = tasks.lock().await;
                    t_lock.pop()
                };
                let (probe_bkh, full_bkh, name, sfz, depth) = match task {
                    Some(t) => t,
                    None => break, // 任务领完
                };

                // 获取空闲浏览器实例
                let (permit, client) = pool.acquire().await;
                
                // 执行撞击提交
                let result = client.query_single(&full_bkh, &sfz).await;
                
                // 雷达探测咬合校验
                let is_hit = match &result {
                    Ok(query_res) => query_res.name == name,
                    Err(_) => false,
                };

                // 释放浏览器回首页待命
                pool.release(permit, client);

                if is_hit {
                    // 触发内部熔断，让其余并发线程迅速安全退出
                    {
                        let mut ic = inner_cancel.lock().await;
                        *ic = true;
                    }
                    let mut l = logs.lock().await;
                    l.push(format!("🎯 [雷达命中] 工人#{} 捕获目标！学生：{}, 考号尾数：{}", worker_id, name, probe_bkh));
                    
                    let _ = tx.send((probe_bkh, depth)).await;
                    break;
                }
            }
        }));
    }

    // 等待回波结果
    let hit_result = rx.recv().await;

    // 强制更改内部状态，确保子线程安全回收
    {
        let mut ic = radar_cancel.lock().await;
        *ic = true; 
    }
    for h in worker_handles { let _ = h.await; }

    hit_result
}

/// ====================================================================
/// 【核心算法阶段二】锁定基地后的矩阵扫射（考号驱动 + 预备身份证动态去无序瘦身）
/// ====================================================================
pub async fn run_matrix_sweep(
    pool: Arc<BrowserPool>,
    students: &mut Vec<(String, String)>, // 传入引用，用于在内部撞中后动态剔除瘦身
    base_bkh: &str,
    bkh_pool: Vec<u64>,                   // 连续的滑窗大考号池
    concurrency: usize,
    cancel_flag: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    logs: Arc<Mutex<Vec<String>>>,
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
) -> Vec<PredictedRecord> {

    // 显式指定类型注解，彻底修复 E0282 编译报错
    let mut final_records: Vec<PredictedRecord> = Vec::new();
    
    let active_students = Arc::new(Mutex::new(students.clone()));
    let resolved_records = Arc::new(Mutex::new(Vec::new()));
    let bkh_queue = Arc::new(Mutex::new(bkh_pool));
    let mut handles = Vec::new();

    for _worker_id in 0..concurrency {
        let pool = pool.clone();
        let bkh_queue = bkh_queue.clone();
        let active_students = active_students.clone();
        let resolved_records = resolved_records.clone();
        let global_cancel = cancel_flag.clone();
        let progress = progress.clone();
        let logs = logs.clone();
        let perf_logs = perf_logs.clone();
        let base_bkh = base_bkh.to_string();

        handles.push(tokio::spawn(async move {
            loop {
                if *global_cancel.lock().await { break; }

                // 领用当前测试的流水考号
                let current_bkh_num = {
                    let mut q = bkh_queue.lock().await;
                    q.pop()
                };
                let current_bkh = match current_bkh_num {
                    Some(num) => num,
                    None => break, // 考号洗完
                };

                let full_exam_number = format!("{}{}", base_bkh, current_bkh);

                // 拷贝当前剩余未解出的死士名单
                let current_batch_students = {
                    let s_lock = active_students.lock().await;
                    s_lock.clone()
                };

                if current_batch_students.is_empty() { break; } // 全班被救完，提早退出

                {
                    let mut p = progress.lock().await;
                    p.current_batch = format!("[扫射阶段] 正在扫射考号 {}（全班剩余{}人待救）", full_exam_number, current_batch_students.len());
                }

                // 用这一个考号，撞击当前剩余的所有人
                for (name, sfz) in current_batch_students {
                    if *global_cancel.lock().await { break; }

                    // 检查此人是否已被别的并发浏览器中途救走
                    {
                        let s_lock = active_students.lock().await;
                        if !s_lock.iter().any(|(_, s)| s == &sfz) { continue; }
                    }

                    let (permit, mut client) = pool.acquire().await;
                    let record_perf = Arc::new(Mutex::new(Vec::new()));
                    client.set_perf(Some(record_perf.clone()));
                    client.set_turbo(true); // 连续矩阵清洗开启极速 Turbo 模式

                    let result = client.query_single(&full_exam_number, &sfz).await;

                    if let Ok(perf_data) = record_perf.try_lock() {
                        if !perf_data.is_empty() {
                            let mut pl = perf_logs.lock().await;
                            pl.push(perf_data.clone());
                        }
                    }
                    pool.release(permit, client);

                    if let Ok(res) = result {
                        if res.name == name {
                            // 动态瘦身：从全局预备库移出
                            {
                                let mut s_lock = active_students.lock().await;
                                s_lock.retain(|(_, s)| s != &sfz);
                            }

                            // 实时同步 UI 面板
                            {
                                let mut p = progress.lock().await;
                                p.matched += 1;
                                p.current_name = name.clone();
                                p.current_exam = full_exam_number.clone();
                            }

                            // 存入成功队列
                            {
                                let mut r_lock = resolved_records.lock().await;
                                r_lock.push(PredictedRecord {
                                    name: name.clone(),
                                    shenfenzheng: sfz.clone(),
                                    exam_number: full_exam_number.clone(),
                                    status: PredictedStatus::Matched,
                                });
                            }

                            let mut l = logs.lock().await;
                            l.push(format!("✨ [狙击成功] 成功解出！学生：{} -> 考号：{}", name, full_exam_number));
                            break; // 考号被占，直接跳出换下一个号
                        }
                    }
                }
            }
        }));
    }

    for h in handles { let _ = h.await; }

    let mut out_records = {
        let r = resolved_records.lock().await;
        r.clone()
    };

    // 扫射完成后依然留存的人，说明在滑窗外，进行 NotFound 状态补齐
    let final_left_students = active_students.lock().await;
    for (name, sfz) in final_left_students.iter() {
        out_records.push(PredictedRecord {
            name: name.clone(),
            shenfenzheng: sfz.clone(),
            exam_number: "滑窗范围外或转学".to_string(),
            status: PredictedStatus::NotFound,
        });
        let mut p = progress.lock().await;
        p.not_found += 1;
    }

    // 回写传出，修剪最外层的班级学生集
    *students = final_left_students.clone();
    out_records
}
