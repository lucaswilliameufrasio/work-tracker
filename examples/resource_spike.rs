//! Run in release, optionally under cgroup limits; output is synthetic evidence only.
#[path = "../tests/support/resource_probe.rs"]
mod resource_probe;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let report = resource_probe::measure(100_000, 10_000, 128).await;
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
