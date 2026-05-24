use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::browser::BrowserPool;
use crate::data::*;

/// Predict a full class by trying each exam number against all remaining students.
/// More efficient: O(N) per class instead of O(N*range).
pub async fn predict_class_batch(
    pool: Arc<BrowserPool>,
    students: Vec<(String, String)>,  // (name, id_card)
    base_bkh: String,
    range_start: u64,
    range_end: u64,
    concurrency: usize,
    cancel: Arc<Mutex<bool>>,
    progress: Arc<Mutex<PredictionProgress>>,
    cache: &mut HashMap<String, String>,  // name -> exam_number (in/out)
    perf_logs: Arc<Mutex<Vec<Vec<PerfEvent>>>>,
) -> Vec<PredictedRecord> {
    let mut results: Vec<PredictedRecord> = Vec::new();
    let n = students.len();
    let total_range = range_end - range_start + 1;

    // Build remaining set: students NOT in cache
    let mut remaining: Vec<(String, String, usize)> = students.iter()
        .enumerate()
        .filter(|(_, (name, _))| !cache.contains_key(name.as_str()))
        .map(|(i, (n, id))| (n.clone(), id.clone(), i))
        .collect();

    // Mark cached students as Matched
    for (name, _) in &students {
        if let Some(bkh) = cache.get(name) {
            results.push(PredictedRecord {
                name: name.clone(),
                shenfenzheng: students.iter().find(|(n,_)| n == name).map(|(_,id)| id.clone()).unwrap_or_default(),
                exam_number: bkh.clone(),
                status: PredictedStatus::Matched,
            });
        }
    }

    {
        let mut p = progress.lock().await;
        p.total = n;
        p.matched = results.len();
        p.not_found = 0;
    }

    if remaining.is_empty() {
        return results;
    }

        // Try each exam number against all remaining students
    for exam_num in range_start..=range_end {
        if *cancel.lock().await { break; }
        if remaining.is_empty() { break; }

        let bkh = format!("{}{:05}", base_bkh, exam_num);

        // Batch try: this exam_num with `concurrency` students at a time
        let mut found = false;
        let chunk_size = concurrency.min(remaining.len());
        let mut i = 0;
        while i < remaining.len() && !found {
            if *cancel.lock().await { break; }

            let end = (i + chunk_size).min(remaining.len());

            // Update batch display
            {
                let batch_names: Vec<String> = remaining[i..end].iter().map(|(n, _, _)| n.clone()).collect();
                let mut p = progress.lock().await;
                p.current_exam = format!("{:05}", exam_num);
                p.current_batch = format!("正在用 {} 等 {} 人 碰撞报考号 {}",
                    batch_names.first().map(|s| s.as_str()).unwrap_or("?"),
                    batch_names.len(),
                    p.current_exam);
            }

            let mut handles = Vec::new();
            for idx in i..end {
                let (name, id_card, _) = &remaining[idx];
                let pool = pool.clone();
                let bkh = bkh.clone();
                let id = id_card.clone();
                let name = name.clone();
                let perf = perf_logs.clone();

                handles.push(tokio::spawn(async move {
                    let (permit, mut client) = pool.acquire().await;
                    let record_perf: Arc<Mutex<Vec<PerfEvent>>> = Arc::new(Mutex::new(Vec::new()));
                    client.set_perf(Some(record_perf.clone()));
                    let result = client.query_single(&bkh, &id).await;
                    if let Ok(pdata) = record_perf.try_lock() {
                        if !pdata.is_empty() {
                            let mut pl = perf.lock().await;
                            pl.push(pdata.clone());
                        }
                    }
                    pool.release(permit, client);
                    (name, id, result)
                }));
            }

            for handle in handles {
                if let Ok((name, id_card, result)) = handle.await {
                    match result {
                        Ok(qr) if !qr.name.is_empty() => {
                            cache.insert(name.clone(), bkh.clone());
                            results.push(PredictedRecord {
                                name: name.clone(),
                                shenfenzheng: id_card.clone(),
                                exam_number: bkh.clone(),
                                status: PredictedStatus::Matched,
                            });
                            remaining.retain(|(n, _, _)| n != &name);
                            {
                                let mut p = progress.lock().await;
                                p.matched += 1;
                                p.current_name = name.clone();
                            }
                            found = true;
                        }
                        _ => {}
                    }
                }
            }
            i = end;
        }
    }

    // Mark unmatched students
    for (name, id_card, _) in &remaining {
        results.push(PredictedRecord {
            name: name.clone(),
            shenfenzheng: id_card.clone(),
            exam_number: String::new(),
            status: PredictedStatus::NotFound,
        });
        let mut p = progress.lock().await;
        p.not_found += 1;
    }

    results
}
