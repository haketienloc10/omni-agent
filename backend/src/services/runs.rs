use std::path::Path;

use sqlx::SqlitePool;

use crate::error::AppError;
use crate::models::run::Run;

/// Read last `max_lines` lines from log file, capped at `max_bytes`.
pub async fn read_log_tail(log_path: &Path, max_lines: usize, max_bytes: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(log_path).await.ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let tail_lines = &lines[lines.len().saturating_sub(max_lines)..];
    let tail = tail_lines.join("\n");
    if tail.len() > max_bytes {
        Some(tail[tail.len() - max_bytes..].to_string())
    } else {
        Some(tail)
    }
}

pub async fn complete_run(
    pool: &SqlitePool,
    run_id: &str,
    exit_code: i32,
    log_tail: Option<&str>,
) -> Result<(), AppError> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE runs SET exit_code = ?, ended_at = ?, log_tail = ? WHERE id = ? AND ended_at IS NULL",
    )
    .bind(exit_code)
    .bind(&now)
    .bind(log_tail)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn normalize_log_tail_for_display(log_tail: &str) -> Option<String> {
    let mut normalized = Vec::new();

    for line in log_tail.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        let Some(root) = value.as_object() else {
            continue;
        };

        if root.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
            normalized.push(trimmed.to_string());
            continue;
        }

        if root.get("type").and_then(|v| v.as_str()) == Some("item.completed") {
            let Some(item) = root.get("item").and_then(|v| v.as_object()) else {
                continue;
            };
            if item.get("type").and_then(|v| v.as_str()) != Some("agent_message") {
                continue;
            }
            let Some(text) = item.get("text").and_then(|v| v.as_str()) else {
                continue;
            };

            normalized.push(
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "agent_message",
                        "message": text,
                        "phase": "final_answer"
                    }
                })
                .to_string(),
            );
            continue;
        }

        if root.get("type").and_then(|v| v.as_str()) == Some("turn.completed") {
            let Some(usage) = root.get("usage").and_then(|v| v.as_object()) else {
                continue;
            };

            normalized.push(
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                                "output_tokens": usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                                "total_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                                    + usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                                "cached_input_tokens": usage.get("cached_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                            }
                        }
                    }
                })
                .to_string(),
            );
        }
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized.join("\n"))
    }
}

fn normalize_run_for_response(mut run: Run) -> Run {
    if let Some(log_tail) = run.log_tail.as_deref()
        && let Some(normalized) = normalize_log_tail_for_display(log_tail)
    {
        run.log_tail = Some(normalized);
    }
    run
}

async fn resolve_project_id(pool: &SqlitePool, project_id: &str) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>("SELECT id FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound {
            code: "project_not_found",
            message: format!("Project {} not found", project_id),
        })
}

async fn verify_task_for_project(
    pool: &SqlitePool,
    project_pk: &str,
    task_id: &str,
) -> Result<(), AppError> {
    let exists: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM tasks WHERE id = ? AND project_id = ?")
            .bind(task_id)
            .bind(project_pk)
            .fetch_one(pool)
            .await?;

    if !exists {
        return Err(AppError::NotFound {
            code: "task_not_found",
            message: format!("Task {} not found", task_id),
        });
    }

    Ok(())
}

pub async fn get_run_by_id(
    pool: &SqlitePool,
    project_id: &str,
    task_id: &str,
    run_id: &str,
) -> Result<Run, AppError> {
    let project_pk = resolve_project_id(pool, project_id).await?;
    verify_task_for_project(pool, &project_pk, task_id).await?;

    let run = sqlx::query_as::<_, Run>(
        "SELECT r.id, r.session_id, r.run_number, r.input, r.exit_code, r.log_path, r.log_tail, r.started_at, r.ended_at \
         FROM runs r \
         INNER JOIN sessions s ON r.session_id = s.id \
         WHERE r.id = ? AND s.task_id = ?",
    )
    .bind(run_id)
    .bind(task_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound {
        code: "run_not_found",
        message: format!("Run {} not found for task {}", run_id, task_id),
    })?;

    Ok(normalize_run_for_response(run))
}

pub async fn list_runs_for_task(
    pool: &SqlitePool,
    project_id: &str,
    task_id: &str,
) -> Result<Vec<Run>, AppError> {
    let project_pk = resolve_project_id(pool, project_id).await?;
    verify_task_for_project(pool, &project_pk, task_id).await?;

    let runs = sqlx::query_as::<_, Run>(
        "SELECT r.id, r.session_id, r.run_number, r.input, r.exit_code, r.log_path, r.log_tail, r.started_at, r.ended_at \
         FROM runs r \
         INNER JOIN sessions s ON r.session_id = s.id \
         WHERE s.task_id = ? \
         ORDER BY r.run_number DESC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;

    Ok(runs.into_iter().map(normalize_run_for_response).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::db::run_migrations;

    fn tmp_log(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("omni-runs-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[tokio::test]
    async fn read_log_tail_returns_last_lines() {
        let path = tmp_log("tail_150.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 1..=150 {
            writeln!(f, "line {}", i).unwrap();
        }
        let tail = read_log_tail(&path, 100, 10_240).await.unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 100);
        assert_eq!(lines[0], "line 51");
        assert_eq!(lines[99], "line 150");
    }

    #[tokio::test]
    async fn read_log_tail_caps_at_max_bytes() {
        let path = tmp_log("tail_bigline.log");
        let mut f = std::fs::File::create(&path).unwrap();
        let long_line = "x".repeat(20_480);
        writeln!(f, "{}", long_line).unwrap();
        let tail = read_log_tail(&path, 100, 10_240).await.unwrap();
        assert!(tail.len() <= 10_240);
    }

    #[tokio::test]
    async fn read_log_tail_returns_none_for_missing_file() {
        let result = read_log_tail(Path::new("/nonexistent/path/to/log.log"), 100, 10_240).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_log_tail_fewer_lines_than_max() {
        let path = tmp_log("tail_10.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {}", i).unwrap();
        }
        let tail = read_log_tail(&path, 100, 10_240).await.unwrap();
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 10);
    }

    #[test]
    fn normalize_log_tail_converts_codex_jsonl_to_frontend_events() {
        let log_tail = [
            r#"{"type":"thread.started","thread_id":"019e6939-6344-7eb2-be93-bd8986505918"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Xin chào. Bạn muốn mình làm gì tiếp?"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":63246,"cached_input_tokens":31744,"output_tokens":62,"reasoning_output_tokens":17}}"#,
        ]
        .join("\n");

        let normalized = normalize_log_tail_for_display(&log_tail).unwrap();

        assert!(!normalized.contains("thread.started"));
        assert!(!normalized.contains("turn.started"));
        assert!(normalized.contains(r#""type":"agent_message""#));
        assert!(normalized.contains(r#""message":"Xin chào. Bạn muốn mình làm gì tiếp?""#));
        assert!(normalized.contains(r#""type":"token_count""#));
        assert!(normalized.contains(r#""input_tokens":63246"#));
        assert!(normalized.contains(r#""output_tokens":62"#));
        assert!(normalized.contains(r#""cached_input_tokens":31744"#));
    }

    #[test]
    fn normalize_log_tail_leaves_plain_output_as_fallback() {
        let log_tail = "Plain agent output\nover multiple lines";
        assert_eq!(normalize_log_tail_for_display(log_tail), None);
    }

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    async fn seed_project_task_session(pool: &SqlitePool) -> (String, String, String) {
        let now = "2026-05-25T10:00:00+00:00";
        let project_id = "proj-1".to_string();
        let task_id = "OMNI-001".to_string();
        let session_id = "session-1".to_string();

        sqlx::query(
            "INSERT INTO projects (id, key, name, created_at, updated_at) VALUES (?, 'OMNI', 'Omni', ?, ?)",
        )
        .bind(&project_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO tasks (id, project_id, seq, title, description, status, created_at, updated_at) \
             VALUES (?, ?, 1, 'Task', 'Desc', 'Paused', ?, ?)",
        )
        .bind(&task_id)
        .bind(&project_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (id, task_id, agent, session_id, status, created_at, last_active) \
             VALUES (?, ?, 'claude', 'agent-session', 'paused', ?, ?)",
        )
        .bind(&session_id)
        .bind(&task_id)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        (project_id, task_id, session_id)
    }

    async fn seed_run(pool: &SqlitePool, session_id: &str, run_id: &str, run_number: i64) {
        sqlx::query(
            "INSERT INTO runs (id, session_id, run_number, input, exit_code, log_path, log_tail, started_at, ended_at) \
             VALUES (?, ?, ?, 'input', 0, '/tmp/run.log', 'tail', '2026-05-25T10:00:00+00:00', '2026-05-25T10:00:30+00:00')",
        )
        .bind(run_id)
        .bind(session_id)
        .bind(run_number)
        .execute(pool)
        .await
        .unwrap();
    }

    fn assert_not_found(err: AppError, expected_code: &str) {
        match err {
            AppError::NotFound { code, .. } => assert_eq!(code, expected_code),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_run_by_id_happy_path() {
        let pool = setup_pool().await;
        let (project_id, task_id, session_id) = seed_project_task_session(&pool).await;
        seed_run(&pool, &session_id, "run-1", 1).await;

        let run = get_run_by_id(&pool, &project_id, &task_id, "run-1")
            .await
            .unwrap();

        assert_eq!(run.id, "run-1");
        assert_eq!(run.run_number, 1);
        assert_eq!(run.input.as_deref(), Some("input"));
        assert_eq!(run.exit_code, Some(0));
    }

    #[tokio::test]
    async fn get_run_by_id_returns_project_not_found_when_project_missing() {
        let pool = setup_pool().await;
        let err = get_run_by_id(&pool, "missing", "OMNI-001", "run-1")
            .await
            .unwrap_err();
        assert_not_found(err, "project_not_found");
    }

    #[tokio::test]
    async fn get_run_by_id_returns_task_not_found_when_task_missing() {
        let pool = setup_pool().await;
        let (project_id, _, _) = seed_project_task_session(&pool).await;
        let err = get_run_by_id(&pool, &project_id, "OMNI-999", "run-1")
            .await
            .unwrap_err();
        assert_not_found(err, "task_not_found");
    }

    #[tokio::test]
    async fn get_run_by_id_returns_run_not_found_when_run_id_unknown() {
        let pool = setup_pool().await;
        let (project_id, task_id, _) = seed_project_task_session(&pool).await;
        let err = get_run_by_id(&pool, &project_id, &task_id, "missing")
            .await
            .unwrap_err();
        assert_not_found(err, "run_not_found");
    }

    #[tokio::test]
    async fn get_run_by_id_returns_run_not_found_when_run_belongs_to_different_task() {
        let pool = setup_pool().await;
        let (project_id, task_id, _) = seed_project_task_session(&pool).await;
        let now = "2026-05-25T10:00:00+00:00";
        sqlx::query(
            "INSERT INTO tasks (id, project_id, seq, title, description, status, created_at, updated_at) \
             VALUES ('OMNI-002', ?, 2, 'Other', 'Desc', 'Paused', ?, ?)",
        )
        .bind(&project_id)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, task_id, agent, session_id, status, created_at, last_active) \
             VALUES ('session-2', 'OMNI-002', 'claude', 'agent-session-2', 'paused', ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        seed_run(&pool, "session-2", "run-other", 1).await;

        let err = get_run_by_id(&pool, &project_id, &task_id, "run-other")
            .await
            .unwrap_err();
        assert_not_found(err, "run_not_found");
    }

    #[tokio::test]
    async fn list_runs_for_task_returns_runs_sorted_desc_by_run_number() {
        let pool = setup_pool().await;
        let (project_id, task_id, session_id) = seed_project_task_session(&pool).await;
        seed_run(&pool, &session_id, "run-2", 2).await;
        seed_run(&pool, &session_id, "run-1", 1).await;
        seed_run(&pool, &session_id, "run-3", 3).await;

        let runs = list_runs_for_task(&pool, &project_id, &task_id)
            .await
            .unwrap();

        let numbers: Vec<i64> = runs.iter().map(|run| run.run_number).collect();
        assert_eq!(numbers, vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn list_runs_for_task_returns_empty_when_no_session() {
        let pool = setup_pool().await;
        let (project_id, task_id, _) = seed_project_task_session(&pool).await;
        sqlx::query("DELETE FROM runs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();

        let runs = list_runs_for_task(&pool, &project_id, &task_id)
            .await
            .unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn list_runs_for_task_returns_project_not_found() {
        let pool = setup_pool().await;
        let err = list_runs_for_task(&pool, "missing", "OMNI-001")
            .await
            .unwrap_err();
        assert_not_found(err, "project_not_found");
    }

    #[tokio::test]
    async fn list_runs_for_task_returns_task_not_found() {
        let pool = setup_pool().await;
        let (project_id, _, _) = seed_project_task_session(&pool).await;
        let err = list_runs_for_task(&pool, &project_id, "OMNI-999")
            .await
            .unwrap_err();
        assert_not_found(err, "task_not_found");
    }

    #[tokio::test]
    async fn list_runs_for_task_does_not_include_runs_of_other_tasks() {
        let pool = setup_pool().await;
        let (project_id, task_id, session_id) = seed_project_task_session(&pool).await;
        seed_run(&pool, &session_id, "run-1", 1).await;
        let now = "2026-05-25T10:00:00+00:00";
        sqlx::query(
            "INSERT INTO tasks (id, project_id, seq, title, description, status, created_at, updated_at) \
             VALUES ('OMNI-002', ?, 2, 'Other', 'Desc', 'Paused', ?, ?)",
        )
        .bind(&project_id)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, task_id, agent, session_id, status, created_at, last_active) \
             VALUES ('session-2', 'OMNI-002', 'claude', 'agent-session-2', 'paused', ?, ?)",
        )
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        seed_run(&pool, "session-2", "run-other", 1).await;

        let runs = list_runs_for_task(&pool, &project_id, &task_id)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "run-1");
    }
}
