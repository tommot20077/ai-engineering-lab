use clap::CommandFactory;
use clap::error::ErrorKind;

use super::*;

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("eval").chain(args.iter().copied()))
}

fn run(args: &[&str]) -> RunArgs {
    match parse(args).unwrap().command {
        Command::Run(run) => run,
        other => panic!("{other:?}"),
    }
}

fn score(args: &[&str]) -> ScoreArgs {
    match parse(args).unwrap().command {
        Command::Score(score) => score,
        other => panic!("{other:?}"),
    }
}

fn kind(args: &[&str]) -> ErrorKind {
    parse(args).unwrap_err().kind()
}

#[test]
fn the_command_definition_is_consistent() {
    // clap 自己的檢查:參數名稱重複、互斥 / 依賴寫到不存在的參數,都會在這裡 panic,而不是等使用者打錯才發現。
    Cli::command().debug_assert();
}

#[test]
fn a_new_run_needs_a_dataset_and_a_model() {
    let r = run(&[
        "run",
        "--dataset",
        "rust-basics",
        "--model",
        "cheap",
        "--repeats",
        "3",
        "--tags",
        "rust,web",
        "--limit",
        "5",
        "--label",
        "new prompt",
        "--concurrency",
        "2",
        "--budget",
        "1.5",
        "--dry-run",
    ]);
    assert_eq!(r.dataset.as_deref(), Some("rust-basics"));
    assert_eq!(r.model.as_deref(), Some("cheap"));
    assert_eq!(r.repeats, 3);
    // 逗號分隔的 tag。
    assert_eq!(r.tags, ["rust", "web"]);
    assert_eq!(
        (r.limit, r.concurrency, r.budget),
        (Some(5), Some(2), Some(1.5))
    );
    assert_eq!(r.label.as_deref(), Some("new prompt"));
    assert!(r.dry_run && r.resume.is_none() && !r.retry_errors);

    assert_eq!(
        kind(&["run", "--model", "cheap"]),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        kind(&["run", "--dataset", "x"]),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn resume_takes_its_settings_from_the_run_and_refuses_new_ones() {
    let r = run(&[
        "run",
        "--resume",
        "/runs/x",
        "--retry-errors",
        "--budget",
        "5",
    ]);
    assert_eq!(r.resume.as_deref(), Some(std::path::Path::new("/runs/x")));
    assert!(r.retry_errors);
    assert_eq!(r.repeats, 1);

    // 接續時設定一律從 meta.json 讀回來:同一次執行不能混兩種設定(design §5)。
    for conflicting in [
        ["--dataset", "x"],
        ["--model", "cheap"],
        ["--repeats", "2"],
        ["--tags", "rust"],
        ["--limit", "3"],
        ["--label", "x"],
    ] {
        let args: Vec<&str> = ["run", "--resume", "/runs/x"]
            .into_iter()
            .chain(conflicting)
            .collect();
        assert_eq!(kind(&args), ErrorKind::ArgumentConflict, "{conflicting:?}");
    }
    // `--retry-errors` 只對接續有意義。
    assert_eq!(
        kind(&["run", "--dataset", "x", "--model", "m", "--retry-errors"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        kind(&["run", "--retry-errors"]),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn numbers_out_of_range_are_rejected_by_the_parser() {
    for (flag, value) in [
        ("--repeats", "0"),
        ("--repeats", "101"),
        ("--concurrency", "0"),
        ("--concurrency", "65"),
        ("--limit", "0"),
    ] {
        let args = ["run", "--dataset", "x", "--model", "m", flag, value];
        assert_eq!(kind(&args), ErrorKind::ValueValidation, "{flag} {value}");
    }
    assert_eq!(
        run(&["run", "--dataset", "x", "--model", "m", "--repeats", "100"]).repeats,
        100
    );
    assert_eq!(
        run(&[
            "run",
            "--dataset",
            "x",
            "--model",
            "m",
            "--concurrency",
            "64"
        ])
        .concurrency,
        Some(64)
    );
    // `--budget=-1`:分開寫的 `--budget -1` 會被 clap 當成另一個參數名稱(一樣被擋下,但測不到值的檢查)。
    for budget in ["0", "-1", "nan", "inf", "abc"] {
        let flag = format!("--budget={budget}");
        let args = ["run", "--dataset", "x", "--model", "m", flag.as_str()];
        assert_eq!(kind(&args), ErrorKind::ValueValidation, "{flag}");
    }
}

#[test]
fn budgets_must_be_positive_finite_dollars() {
    assert_eq!(positive_usd("2.5"), Ok(2.5));
    assert_eq!(positive_usd("0.01"), Ok(0.01));
    for bad in ["0", "-1", "nan", "inf", "-inf", "", "two"] {
        assert!(positive_usd(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        positive_usd("0"),
        Err("must be a positive number of US dollars, like 2.00".to_owned())
    );
}

#[test]
fn score_defaults_to_the_configured_judge_and_names_the_scoring_after_it() {
    let s = score(&["score", "/runs/x"]);
    assert_eq!(s.run, std::path::Path::new("/runs/x"));
    assert_eq!(s.judge_alias("default"), Some("default"));
    assert_eq!(s.score_name("default"), "default");

    let s = score(&["score", "/runs/x", "--judge", "sonnet", "--judge-all"]);
    assert_eq!(s.judge_alias("default"), Some("sonnet"));
    assert_eq!(s.score_name("default"), "sonnet");
    assert!(s.judge_all);

    let s = score(&["score", "/runs/x", "--no-judge"]);
    assert_eq!(s.judge_alias("default"), None);
    assert_eq!(s.score_name("default"), CHECKS_ONLY);

    let s = score(&["score", "/runs/x", "--judge", "sonnet", "--name", "v2"]);
    assert_eq!(s.score_name("default"), "v2");
}

#[test]
fn no_judge_conflicts_with_judge_options() {
    assert_eq!(
        kind(&["score", "/runs/x", "--no-judge", "--judge", "default"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        kind(&["score", "/runs/x", "--no-judge", "--judge-all"]),
        ErrorKind::ArgumentConflict
    );
    assert_eq!(kind(&["score"]), ErrorKind::MissingRequiredArgument);
}

#[test]
fn compare_takes_two_runs() {
    let Command::Compare(c) = parse(&["compare", "/a", "/b", "--scores", "j", "--allow-mismatch"])
        .unwrap()
        .command
    else {
        panic!()
    };
    assert_eq!(
        (c.run_a.to_str(), c.run_b.to_str()),
        (Some("/a"), Some("/b"))
    );
    assert_eq!(c.scores.as_deref(), Some("j"));
    assert!(c.allow_mismatch);
    assert_eq!(kind(&["compare", "/a"]), ErrorKind::MissingRequiredArgument);
}

#[test]
fn the_config_option_works_before_or_after_the_subcommand() {
    assert_eq!(
        parse(&["compare", "/a", "/b"]).unwrap().config,
        std::path::Path::new("eval.toml")
    );
    assert_eq!(
        parse(&["--config", "x.toml", "compare", "/a", "/b"])
            .unwrap()
            .config,
        std::path::Path::new("x.toml")
    );
    assert_eq!(
        parse(&["compare", "/a", "/b", "--config", "y.toml"])
            .unwrap()
            .config,
        std::path::Path::new("y.toml")
    );
}

#[test]
fn judge_all_turns_off_skipping_for_one_scoring() {
    // (cargo-mutants:這段原本在 main.rs,`&&` 換成 `||`、拿掉 `!` 都沒人發現。)
    let plain = score(&["score", "/runs/x"]);
    let all = score(&["score", "/runs/x", "--judge-all"]);
    assert!(plain.skip_on_check_failure(true));
    assert!(!plain.skip_on_check_failure(false));
    assert!(!all.skip_on_check_failure(true));
    assert!(!all.skip_on_check_failure(false));
}
