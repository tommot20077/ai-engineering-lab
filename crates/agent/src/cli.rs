//! 命令列(design §8)。
//!
//! ```text
//! agent run "問題" [--model default] [--root <目錄>] [--system <檔案>] [--max-turns 10] [--no-record]
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
        ])
        .unwrap();
        assert_eq!(cli.config, PathBuf::from("x.toml"));
        let Command::Run(args) = cli.command;
        assert_eq!(args.model.as_deref(), Some("cheap"));
        assert_eq!(args.root, Some(PathBuf::from("notes")));
        assert_eq!(args.system, Some(PathBuf::from("prompt.md")));
        assert_eq!(args.max_turns, Some(3));
        assert!(args.no_record);
    }

    #[test]
    fn zero_turns_is_refused_by_the_parser() {
        assert!(parse(&["run", "task", "--max-turns", "0"]).is_err());
        assert!(parse(&["run", "task", "--max-turns", "1"]).is_ok());
        assert!(parse(&["run"]).is_err());
    }
}
