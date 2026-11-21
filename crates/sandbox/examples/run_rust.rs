//! 在沙箱裡編譯並執行一個 Rust 檔案。
//!
//! ```bash
//! docker pull "$(cargo run -q -p sandbox --example run_rust -- --image)"   # 第一次:下載鎖定的 image
//! cargo run -p sandbox --example run_rust -- path/to/main.rs
//! cargo run -p sandbox --example run_rust -- --compile-only path/to/main.rs
//! ```

use std::process::ExitCode;

use sandbox::{Action, ContainerConfig, ContainerSandbox, DEFAULT_IMAGE, Output, Sandbox};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (action, path) = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--image"] => {
            println!("{DEFAULT_IMAGE}");
            return ExitCode::SUCCESS;
        }
        ["--compile-only", path] => (Action::Compile, path.to_string()),
        [path] => (Action::Run, path.to_string()),
        _ => {
            eprintln!("usage: run_rust [--compile-only] <file.rs> | --image");
            return ExitCode::FAILURE;
        }
    };
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let sandbox = match ContainerSandbox::new(ContainerConfig::default()) {
        Ok(sandbox) => sandbox,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match sandbox.rust(&source, action).await {
        Ok(report) => {
            show("compile", &report.compile);
            if let Some(run) = &report.run {
                show("run", run);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn show(step: &str, output: &Output) {
    let status = match (output.timed_out, output.exit_code) {
        (true, _) => "timed out".to_owned(),
        (false, Some(code)) => format!("exit code {code}"),
        (false, None) => "killed".to_owned(),
    };
    let truncated = if output.truncated {
        " (output truncated)"
    } else {
        ""
    };
    println!("== {step}: {status}{truncated}");
    if !output.stdout.is_empty() {
        println!("-- stdout\n{}", output.stdout.trim_end());
    }
    if !output.stderr.is_empty() {
        println!("-- stderr\n{}", output.stderr.trim_end());
    }
}
