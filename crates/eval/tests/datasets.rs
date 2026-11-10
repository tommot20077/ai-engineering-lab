//! 公開資料集(repo 的 `evals/`)一定要讀得進來:手寫的檔案改壞了,要在 CI 抓到,而不是等到花錢跑模型時才發現。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use eval::Dataset;

#[test]
fn every_public_dataset_is_valid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            // `Dataset::load` 會一次列出全部的問題(重複 id、未知欄位、沒有檢查也沒有評分標準……)。
            let ds = Dataset::load(&path).unwrap_or_else(|e| panic!("{e}"));
            assert!(!ds.cases.is_empty(), "{} is empty", path.display());
            names.push(ds.name);
        }
    }
    names.sort();
    assert_eq!(names, ["rust-basics", "smoke"]);
}

#[test]
fn the_smoke_dataset_needs_no_judge() {
    // CI 的 E2E 用 `eval score --no-judge` 打分:smoke 不能有評分標準(否則會被拒絕)。
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/smoke.jsonl");
    let ds = Dataset::load(&path).unwrap();
    assert!(
        ds.cases
            .iter()
            .all(|c| c.rubric.is_empty() && !c.checks.is_empty())
    );
}
