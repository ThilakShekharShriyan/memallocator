use std::time::Duration;

fn main() {
    let command = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    match command.as_str() {
        "all" => memallocator::bench::run(Duration::from_millis(200), true),
        "cpu" => memallocator::bench::run_cpu(Duration::from_millis(200)),
        "gpu" => {
            if !memallocator::bench::run_gpu(true) {
                std::process::exit(1);
            }
        }
        "quick" => memallocator::bench::run(Duration::from_millis(80), false),
        "help" | "--help" | "-h" => help(),
        other => {
            eprintln!("unknown command {other}\n");
            help();
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(
        "memallocator

  cargo run --release
  cargo run --release -- cpu
  cargo run --release -- gpu
  cargo run --release -- quick
  cargo test"
    );
}
