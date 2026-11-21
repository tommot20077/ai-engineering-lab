//! `run_rust`:在沙箱裡編譯並執行模型寫的單檔 Rust 程式(003 第三部分第 5 步,D-053、D-054)。
//!
//! 只在 `--allow-run` 時註冊;每次呼叫都要核准(看得到完整的程式)。工具只拿到 `Arc<dyn Sandbox>`:
//! 不知道底下是 docker、podman,還是測試用的假沙箱。

use std::fmt::Write as _;
use std::sync::Arc;

use sandbox::{Action, Output, Report, Sandbox};
use schemars::JsonSchema;
use serde::Deserialize;

use super::write::preview_capped;
use crate::tool::{Tool, ToolContext, ToolError};

/// 核准提示裡最多顯示幾行程式、幾個 bytes。程式會被執行,盡量讓人看完整(沙箱才是真正的防線)。
const CODE_PREVIEW_LINES: usize = 200;
const CODE_PREVIEW_BYTES: usize = 16 * 1024;

/// `run_rust` 工具。
pub struct RunRust {
    sandbox: Arc<dyn Sandbox>,
    description: String,
}

impl RunRust {
    /// `compile_secs`、`run_secs` 寫進給模型看的說明(上限是設定的,模型要知道)。
    // `Arc<dyn Sandbox>`:trait object。Java 的 `Sandbox sandbox` 欄位(介面型別)就是這個意思;
    // Rust 要明說「這是一個指向某個實作的指標」(`dyn`),而且要說誰擁有它(`Arc`:可以共用,eval 的每一題都拿一份)。
    pub fn new(sandbox: Arc<dyn Sandbox>, compile_secs: u64, run_secs: u64) -> RunRust {
        let description = format!(
            "在隔離的沙箱裡編譯並執行一個單檔 Rust 程式(要有 fn main;edition 2024;只有標準庫,不能用外部 crate)。\
             沙箱沒有網路、看不到使用者的檔案;編譯最多 {compile_secs} 秒、執行最多 {run_secs} 秒。\
             回傳編譯錯誤,或程式的結束碼與輸出。每次執行都要使用者核准,使用者可能拒絕。"
        );
        RunRust {
            sandbox,
            description,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunRustArgs {
    /// 完整的 Rust 原始碼(單一檔案,要有 `fn main`)。
    pub code: String,
}

impl Tool for RunRust {
    type Args = RunRustArgs;

    fn name(&self) -> &str {
        "run_rust"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn needs_approval(&self) -> bool {
        true
    }

    async fn describe(&self, args: &RunRustArgs, _ctx: &ToolContext) -> Option<String> {
        let numbered: String = args
            .code
            .lines()
            .zip(1..)
            .map(|(line, n)| format!("{n:>4} | {line}\n"))
            .collect();
        Some(format!(
            "compile and run this Rust program in the sandbox (no network, no access to your files):\n{}",
            preview_capped(&numbered, "", CODE_PREVIEW_LINES, CODE_PREVIEW_BYTES)
        ))
    }

    async fn call(&self, args: RunRustArgs, _ctx: &ToolContext) -> Result<String, ToolError> {
        let report = self
            .sandbox
            .rust(&args.code, Action::Run)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(render(&report))
    }
}

/// 給模型看的結果:編譯與執行分開寫;輸出原樣附上。
fn render(report: &Report) -> String {
    let mut out = String::new();
    let compile = &report.compile;
    if compile.success() {
        out.push_str("compile: ok\n");
    } else {
        // `let _ =`:寫進 String 不會失敗,`write!` 的 `Result` 只是形式上要處理。
        let _ = writeln!(out, "compile: failed ({})", status(compile));
        append_streams(&mut out, compile);
        return out.trim_end().to_owned();
    }
    if let Some(run) = &report.run {
        let _ = writeln!(out, "run: {}", status(run));
        append_streams(&mut out, run);
    }
    out.trim_end().to_owned()
}

fn status(output: &Output) -> String {
    match (output.timed_out, output.exit_code) {
        (true, _) => "timed out and was stopped".to_owned(),
        // 137 = 128 + SIGKILL:沙箱裡最常見的原因是記憶體用完(OOM killer)。
        (false, Some(137)) => "exit code 137: killed (probably out of memory)".to_owned(),
        (false, Some(code)) => format!("exit code {code}"),
        (false, None) => "killed by a signal".to_owned(),
    }
}

fn append_streams(out: &mut String, output: &Output) {
    for (name, text) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        if !text.is_empty() {
            let _ = writeln!(out, "{name}:\n{}", text.trim_end());
        }
    }
    if output.truncated {
        out.push_str("(output truncated)\n");
    }
}

#[cfg(test)]
mod tests;
