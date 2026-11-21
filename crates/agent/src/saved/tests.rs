use llm::{ProviderMetadata, Response, StopKind, StopReason, Usage};

use super::*;
use crate::AgentLimits;
use crate::run::{SUSPENDED_VERSION, Step};

fn sample(run_id: Uuid) -> SavedRun {
    SavedRun {
        plan: RunPlan {
            model: "default".into(),
            system: "你是助理".into(),
            max_turns: Some(3),
            root: Some("/home/me/notes".into()),
            allow_write: true,
            allow_run: false,
            record: false,
        },
        trajectory: "/home/me/runs/x.jsonl".into(),
        suspended: Suspended {
            version: SUSPENDED_VERSION,
            run_id,
            model: "default".into(),
            system: Some("你是助理".into()),
            max_tokens: 1000,
            limits: AgentLimits::default(),
            tools: Vec::new(),
            messages: Vec::new(),
            steps: Vec::new(),
            usage: Usage::default(),
            tool_calls: 0,
            call_ids: Vec::new(),
            used_ms: 1234,
            waiting: Step {
                turn: 1,
                response: Response {
                    id: None,
                    model: "m".into(),
                    content: Vec::new(),
                    stop: StopReason {
                        kind: StopKind::ToolUse,
                        raw: "tool_use".into(),
                    },
                    usage: Usage::default(),
                    meta: ProviderMetadata::default(),
                },
                latency_ms: 5,
                tool_calls: Vec::new(),
            },
            pending: Vec::new(),
        },
    }
}

#[test]
fn a_saved_run_is_read_back_unchanged_from_a_private_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs").join(SUSPENDED_DIR);
    let id = Uuid::now_v7();
    let file = save(&dir, &sample(id)).unwrap();
    assert_eq!(file, dir.join(format!("{id}.json")));
    assert_eq!(load(&dir, id).unwrap(), sample(id));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
    }
}

#[test]
fn a_saved_run_is_never_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s");
    let id = Uuid::now_v7();
    save(&dir, &sample(id)).unwrap();
    let mut other = sample(id);
    other.plan.model = "cheap".into();
    let error = save(&dir, &other).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(load(&dir, id).unwrap().plan.model, "default");
}

#[test]
fn only_one_claim_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s");
    let id = Uuid::now_v7();
    save(&dir, &sample(id)).unwrap();
    claim(&dir, id).unwrap();
    assert!(matches!(claim(&dir, id), Err(SavedError::NotFound(found)) if found == id));
    assert!(matches!(load(&dir, id), Err(SavedError::NotFound(_))));
}

#[test]
fn a_missing_or_broken_file_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let id = Uuid::nil();
    assert_eq!(
        load(tmp.path(), id).unwrap_err().to_string(),
        "there is no paused run 00000000-0000-0000-0000-000000000000"
    );
    std::fs::write(path(tmp.path(), id), "{not json").unwrap();
    let error = load(tmp.path(), id).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!(
            "the saved run {} cannot be read: ",
            path(tmp.path(), id).display()
        )),
        "{error}"
    );
}

#[test]
fn unknown_fields_in_a_saved_run_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let id = Uuid::nil();
    let mut value = serde_json::to_value(sample(id)).unwrap();
    value["plan"]["allow_shell"] = serde_json::json!(true);
    std::fs::write(path(tmp.path(), id), value.to_string()).unwrap();
    assert!(matches!(
        load(tmp.path(), id),
        Err(SavedError::Unreadable { .. })
    ));
}
