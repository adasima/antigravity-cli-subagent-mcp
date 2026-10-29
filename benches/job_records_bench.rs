use std::time::Instant;

fn setup() {
    let _ = std::fs::create_dir_all(".agy/jobs");
    for i in 0..10000 {
        let path = format!(".agy/jobs/j{i}.json");
        std::fs::write(&path, r#"{"id":"j1","workspace":"/tmp","prompt":"hi","started_unix":123,"status":"running","seen":false}"#).unwrap();
    }
}

fn teardown() {
    let _ = std::fs::remove_dir_all(".agy/jobs");
}

fn main() {
    std::env::set_var("AGY_DIR", ".agy");
    setup();

    let jobs_dir = ".agy/jobs";

    // Baseline
    let start_sync = Instant::now();
    let v_sync: Vec<serde_json::Value> = std::fs::read_dir(jobs_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .collect();
    let duration_sync = start_sync.elapsed();

    println!("Baseline duration for 10000 records: {:?}", duration_sync);
    println!("Read {} records", v_sync.len());

    // Parallel using standard threads
    let start_thread = Instant::now();

    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(jobs_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| e.path())
        .collect();

    let chunk_size = (entries.len() / 8).max(1);
    let mut handles = vec![];

    for chunk in entries.chunks(chunk_size) {
        let chunk = chunk.to_vec();
        handles.push(std::thread::spawn(move || {
            let mut local_v = Vec::new();
            for path in chunk {
                if let Ok(s) = std::fs::read_to_string(path) {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&s) {
                        local_v.push(val);
                    }
                }
            }
            local_v
        }));
    }

    let mut v_thread = Vec::new();
    for handle in handles {
        v_thread.extend(handle.join().unwrap());
    }

    let duration_thread = start_thread.elapsed();

    println!("Threaded duration for 10000 records: {:?}", duration_thread);
    println!("Read {} records", v_thread.len());

    teardown();
}
