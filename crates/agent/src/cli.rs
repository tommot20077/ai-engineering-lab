//! 命令列(design §8)。
//!
//! ```text
//! agent run "問題" [--model default] [--root <目錄>] [--allow-write] [--yes] [--system <檔案>] [--max-turns 10] [--no-record]
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// 設定檔的預設位置(目前目錄)。
pub const DEFAULT_CONFIG: &str = "agent.toml";

#[derive(Debug, Parser)]
#[command(name = "agent", version, about = "Run a tool-using agent on one task")]
pub struct Cli {
    // `///` 會變成 `--help` 的說明,跟其他 CLI 輸出一樣用英文。
    /// Config file (relative to the current directory).
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Answer one task, calling tools as needed; print the answer.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The task, as you would ask it.
    pub task: String,

    /// Model alias from [aliases] (default: [agent] model).
    #[arg(long)]
    pub model: Option<String>,

    /// Directory the file tools (read_file, list_dir) may read; without it they are not offered.
    #[arg(long)]
    pub root: Option<PathBuf>,

    /// Also offer write_file and edit_file inside --root. Every write asks for your approval first.
    // `requires`:沒有 `--root` 就給 `--allow-write`,clap 直接報錯(寫到哪裡?)。
    #[arg(long, requires = "root")]
    pub allow_write: bool,

    /// Approve every tool call without asking (for scripts; you will not see what is written first).
    #[arg(long)]
    pub yes: bool,

    /// File with a system prompt to use instead of the built-in one.
    #[arg(long)]
    pub system: Option<PathBuf>,

    /// Override [limits] max_turns for this run.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_turns: Option<u32>,

    /// Do not write this run's model calls to the call records.
    #[arg(long)]
    pub no_record: bool,
}

/// 核准提示的回答:`y` / `yes`(不分大小寫、前後空白不算)才是核准,其他一律拒絕(包括空白行與讀不到)。
pub fn approves(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// 把模型給的文字印到終端機之前先整理:控制字元(ESC 開頭的控制序列可以清畫面、改標題、偽造輸出)
/// 與會改變文字方向、看不見的 Unicode 字元(例如 U+202E,讓 `txt.exe` 看起來像 `exe.txt`)都換成 `?`。
/// 換行與 tab 保留(說明本來就有多行)。核准的人看到的必須就是會發生的事。
pub fn terminal_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '?',
            // 零寬字元、方向控制(LRM、RLM、LRE…RLO、LRI…PDI)、BOM。
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}' => '?',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("agent").chain(args.iter().copied()))
    }

    #[test]
    fn a_task_is_all_that_is_required() {
        let cli = parse(&["run", "幾點了?"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("agent.toml"));
        let Command::Run(args) = cli.command;
        assert_eq!(args.task, "幾點了?");
        assert_eq!(args.model, None);
        assert_eq!(args.root, None);
        assert!(!args.allow_write);
        assert!(!args.yes);
        assert!(!args.no_record);
    }

    #[test]
    fn every_option_is_parsed() {
        let cli = parse(&[
            "--config",
            "x.toml",
            "run",
            "task",
            "--model",
            "cheap",
            "--root",
            "notes",
            "--system",
            "prompt.md",
            "--max-turns",
            "3",
            "--no-record",
            "--allow-write",
            "--yes",
        ])
        .unwrap();
        assert_eq!(cli.config, PathBuf::from("x.toml"));
        let Command::Run(args) = cli.command;
        assert_eq!(args.model.as_deref(), Some("cheap"));
        assert_eq!(args.root, Some(PathBuf::from("notes")));
        assert_eq!(args.system, Some(PathBuf::from("prompt.md")));
        assert_eq!(args.max_turns, Some(3));
        assert!(args.no_record);
        assert!(args.allow_write);
        assert!(args.yes);
    }

    #[test]
    fn allow_write_needs_a_root() {
        let error = parse(&["run", "task", "--allow-write"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(parse(&["run", "task", "--allow-write", "--root", "notes"]).is_ok());
    }

    #[test]
    fn only_yes_approves() {
        for answer in ["y", "Y", "yes", " YES \n", "y\r\n"] {
            assert!(approves(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yess", "y y", "ok", "是"] {
            assert!(!approves(answer), "{answer:?}");
        }
    }

    #[test]
    fn text_from_the_model_is_made_safe_for_the_terminal() {
        assert_eq!(
            terminal_safe("line 1\n\tline 2\x1b[2J\x07\r"),
            "line 1\n\tline 2?[2J??"
        );
        assert_eq!(terminal_safe("notes\u{202E}txt.exe"), "notes?txt.exe");
        assert_eq!(terminal_safe("a\u{200B}b\u{2066}c\u{FEFF}"), "a?b?c?");
        assert_eq!(terminal_safe("中文 é 🦀"), "中文 é 🦀");
    }

    #[test]
    fn zero_turns_is_refused_by_the_parser() {
        assert!(parse(&["run", "task", "--max-turns", "0"]).is_err());
        assert!(parse(&["run", "task", "--max-turns", "1"]).is_ok());
        assert!(parse(&["run"]).is_err());
    }
}
