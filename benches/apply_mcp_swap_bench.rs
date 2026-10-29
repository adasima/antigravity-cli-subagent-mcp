use std::time::Instant;

#[tokio::main]
async fn main() {
    let start = Instant::now();
    let iters = 1000;

    for _ in 0..iters {
        // simulate the std::fs approach
        let _ = std::fs::write("/tmp/test_sync.txt", "test");
        let _ = std::fs::read_to_string("/tmp/test_sync.txt");
        let _ = std::fs::remove_file("/tmp/test_sync.txt");
    }
    let duration_sync = start.elapsed();

    let start = Instant::now();
    for _ in 0..iters {
        // simulate the tokio::fs approach
        let _ = tokio::fs::write("/tmp/test_async.txt", "test").await;
        let _ = tokio::fs::read_to_string("/tmp/test_async.txt").await;
        let _ = tokio::fs::remove_file("/tmp/test_async.txt").await;
    }
    let duration_async = start.elapsed();

    println!("Baseline (std::fs): {:?}", duration_sync);
    println!("Optimized (tokio::fs): {:?}", duration_async);
}
