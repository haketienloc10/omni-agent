use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use serde_json::Value;
use serial_test::serial;
use sqlx::{Row, SqlitePool};
use tokio::sync::Mutex;
use tower::ServiceExt;

use omni_agent_backend::{db, error::AppError, handlers, services, state::AppState};

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn fallback_handler() -> impl IntoResponse {
    AppError::NotFound {
        code: "not_found",
        message: "Route not found".to_string(),
    }
}

async fn build_runs_app() -> (Router, Arc<AppState>) {
    let pool = SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db::run_migrations(&pool).await.unwrap();

    let state = Arc::new(AppState {
        db: pool,
        subprocess_map: Arc::new(Mutex::new(HashMap::new())),
        agent_config_path: std::env::temp_dir()
            .join(format!("omni-agent-test-{}.json", uuid::Uuid::new_v4())),
    });

    let api_router = Router::new()
        .route(
            "/projects",
            get(handlers::projects::list_projects).post(handlers::projects::create_project),
        )
        .route(
            "/projects/{project_id}/tasks",
            get(handlers::tasks::list_tasks).post(handlers::tasks::create_task),
        )
        .route(
            "/projects/{project_id}/tasks/{task_id}",
            get(handlers::tasks::get_task),
        )
        .route(
            "/projects/{project_id}/tasks/{task_id}/assign",
            axum::routing::post(handlers::tasks::assign_agent),
        )
        .route(
            "/projects/{project_id}/tasks/{task_id}/sessions/start",
            axum::routing::post(handlers::sessions::start_session),
        )
        .route(
            "/projects/{project_id}/tasks/{task_id}/runs",
            get(handlers::runs::list_runs),
        )
        .route(
            "/projects/{project_id}/tasks/{task_id}/runs/{run_id}",
            get(handlers::runs::get_run),
        );

    let app = Router::new()
        .route("/health", get(health_handler))
        .nest("/api", api_router)
        .fallback(fallback_handler)
        .with_state(state.clone());

    (app, state)
}

async fn body_json(body: Body) -> Value {
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

fn fixture(name: &str) -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    format!("{}/tests/fixtures/{}", manifest, name)
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
         VALUES (?, ?, ?, NULL, 0, '/tmp/run.log', 'Last lines\n[stderr] some warning\n', '2026-05-25T10:00:00+00:00', '2026-05-25T10:00:30+00:00')",
    )
    .bind(run_id)
    .bind(session_id)
    .bind(run_number)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_run_with_log_tail(pool: &SqlitePool, session_id: &str, run_id: &str, log_tail: &str) {
    sqlx::query(
        "INSERT INTO runs (id, session_id, run_number, input, exit_code, log_path, log_tail, started_at, ended_at) \
         VALUES (?, ?, 1, NULL, 0, '/tmp/run.log', ?, '2026-05-25T10:00:00+00:00', '2026-05-25T10:00:30+00:00')",
    )
    .bind(run_id)
    .bind(session_id)
    .bind(log_tail)
    .execute(pool)
    .await
    .unwrap();
}

async fn setup_assigned_task(app: &Router, agent: &str) -> (String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/projects")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"name": "OmniAgent", "key": "OMNI", "workspacePath": "/tmp"})
                .to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let body = body_json(res.into_body()).await;
    let project_id = body["id"].as_str().unwrap().to_string();

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/projects/{}/tasks", project_id))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"title": "Fix login", "description": "Token broken", "agent": "claude", "role": "coder"}"#,
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let body = body_json(res.into_body()).await;
    let task_id = body["id"].as_str().unwrap().to_string();

    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/projects/{}/tasks/{}/assign",
            project_id, task_id
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"agent": agent, "role": "coder"}).to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    (project_id, task_id)
}

async fn get_json(app: &Router, uri: String) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = body_json(res.into_body()).await;
    (status, body)
}

#[tokio::test]
async fn get_run_by_id_returns_200_with_camelcase_body() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, session_id) = seed_project_task_session(&state.db).await;
    seed_run(&state.db, &session_id, "run-1", 1).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs/run-1"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {}", body);
    for key in [
        "id",
        "runNumber",
        "input",
        "exitCode",
        "logPath",
        "logTail",
        "startedAt",
        "endedAt",
    ] {
        assert!(body.get(key).is_some(), "missing key {key}: {body}");
    }
    assert!(body.get("sessionId").is_none());
    assert!(body.get("run_number").is_none());
    assert_eq!(body["id"], "run-1");
    assert_eq!(body["runNumber"], 1);
}

#[tokio::test]
async fn get_run_by_id_normalizes_codex_jsonl_log_tail_for_chat_view() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, session_id) = seed_project_task_session(&state.db).await;
    let codex_log_tail = [
        r#"{"type":"thread.started","thread_id":"019e6939-6344-7eb2-be93-bd8986505918"}"#,
        r#"{"type":"turn.started"}"#,
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Xin chào. Bạn muốn mình làm gì tiếp?"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":63246,"cached_input_tokens":31744,"output_tokens":62,"reasoning_output_tokens":17}}"#,
    ]
    .join("\n");
    seed_run_with_log_tail(&state.db, &session_id, "run-codex", &codex_log_tail).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs/run-codex"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {}", body);
    let log_tail = body["logTail"].as_str().unwrap();
    assert!(!log_tail.contains("thread.started"));
    assert!(!log_tail.contains("turn.started"));
    assert!(log_tail.contains(r#""type":"agent_message""#));
    assert!(log_tail.contains(r#""message":"Xin chào. Bạn muốn mình làm gì tiếp?""#));
    assert!(log_tail.contains(r#""type":"token_count""#));
}

#[tokio::test]
async fn get_run_by_id_returns_404_run_not_found() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, _) = seed_project_task_session(&state.db).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs/missing"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "run_not_found");
}

#[tokio::test]
async fn get_run_by_id_returns_404_when_run_belongs_to_other_task() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, _) = seed_project_task_session(&state.db).await;
    let now = "2026-05-25T10:00:00+00:00";
    sqlx::query(
        "INSERT INTO tasks (id, project_id, seq, title, description, status, created_at, updated_at) \
         VALUES ('OMNI-002', ?, 2, 'Other', 'Desc', 'Paused', ?, ?)",
    )
    .bind(&project_id)
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (id, task_id, agent, session_id, status, created_at, last_active) \
         VALUES ('session-2', 'OMNI-002', 'claude', 'agent-session-2', 'paused', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(&state.db)
    .await
    .unwrap();
    seed_run(&state.db, "session-2", "run-other", 1).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs/run-other"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "run_not_found");
}

#[tokio::test]
async fn get_run_by_id_returns_404_project_not_found() {
    let (app, _) = build_runs_app().await;
    let (status, body) = get_json(
        &app,
        "/api/projects/missing/tasks/OMNI-001/runs/run-1".to_string(),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "project_not_found");
}

#[tokio::test]
async fn get_run_by_id_returns_404_task_not_found() {
    let (app, state) = build_runs_app().await;
    let (project_id, _, _) = seed_project_task_session(&state.db).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/OMNI-999/runs/run-1"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "task_not_found");
}

#[tokio::test]
async fn list_runs_returns_sorted_desc() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, session_id) = seed_project_task_session(&state.db).await;
    seed_run(&state.db, &session_id, "run-2", 2).await;
    seed_run(&state.db, &session_id, "run-1", 1).await;
    seed_run(&state.db, &session_id, "run-3", 3).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let runs = body.as_array().unwrap();
    let numbers: Vec<i64> = runs
        .iter()
        .map(|run| run["runNumber"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, vec![3, 2, 1]);
}

#[tokio::test]
async fn list_runs_returns_empty_array_when_no_session() {
    let (app, state) = build_runs_app().await;
    let (project_id, task_id, _) = seed_project_task_session(&state.db).await;
    sqlx::query("DELETE FROM sessions")
        .execute(&state.db)
        .await
        .unwrap();

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/{task_id}/runs"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn list_runs_returns_404_project_not_found() {
    let (app, _) = build_runs_app().await;
    let (status, body) = get_json(
        &app,
        "/api/projects/missing/tasks/OMNI-001/runs".to_string(),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "project_not_found");
}

#[tokio::test]
async fn list_runs_returns_404_task_not_found() {
    let (app, state) = build_runs_app().await;
    let (project_id, _, _) = seed_project_task_session(&state.db).await;

    let (status, body) = get_json(
        &app,
        format!("/api/projects/{project_id}/tasks/OMNI-999/runs"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "task_not_found");
}

#[tokio::test]
async fn runs_persist_after_pool_reopen() {
    let db_dir = std::env::temp_dir().join(format!(
        "omni-runs-db-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("test.db");
    let db_url = format!("sqlite://{}", db_path.display());

    let pool1 = SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(&db_url)
            .unwrap()
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db::run_migrations(&pool1).await.unwrap();
    let (project_id, task_id, session_id) = seed_project_task_session(&pool1).await;
    seed_run(&pool1, &session_id, "run-1", 1).await;
    pool1.close().await;

    let pool2 = SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::from_str(&db_url)
            .unwrap()
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db::run_migrations(&pool2).await.unwrap();

    let runs = services::runs::list_runs_for_task(&pool2, &project_id, &task_id)
        .await
        .unwrap();

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "run-1");
    assert_eq!(runs[0].exit_code, Some(0));
    assert_eq!(
        runs[0].log_tail.as_deref(),
        Some("Last lines\n[stderr] some warning\n")
    );

    pool2.close().await;
    std::fs::remove_dir_all(db_dir).ok();
}

#[tokio::test]
#[serial]
async fn start_session_writes_both_stdout_and_stderr_to_log_file() {
    let tmp_home = std::env::temp_dir().join(format!(
        "omni-runs-home-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    std::fs::create_dir_all(&tmp_home).unwrap();
    unsafe {
        std::env::set_var("HOME", tmp_home.to_str().unwrap());
        std::env::set_var("OMNI_AGENT_CLAUDE_BIN", fixture("mock-agent-stderr.sh"));
    }

    let (app, state) = build_runs_app().await;
    let (project_id, task_id) = setup_assigned_task(&app, "claude").await;

    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/projects/{project_id}/tasks/{task_id}/sessions/start"
        ))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    tokio::time::sleep(tokio::time::Duration::from_millis(1200)).await;

    let row = sqlx::query("SELECT log_path, log_tail FROM runs LIMIT 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let log_path: String = row.try_get("log_path").unwrap();
    let content = std::fs::read_to_string(&log_path).unwrap();

    assert!(content.contains("stdout line 1"), "{content}");
    assert!(content.contains("stdout line 2"), "{content}");
    assert!(content.contains("stdout line 3"), "{content}");
    assert!(content.contains("[stderr] stderr line A"), "{content}");
    assert!(content.contains("[stderr] stderr line B"), "{content}");

    let log_tail: Option<String> = row.try_get("log_tail").unwrap();
    assert!(
        log_tail
            .as_deref()
            .unwrap_or_default()
            .contains("[stderr] stderr line B"),
        "tail: {:?}",
        log_tail
    );

    std::fs::remove_dir_all(&tmp_home).ok();
    unsafe {
        std::env::remove_var("HOME");
        std::env::remove_var("OMNI_AGENT_CLAUDE_BIN");
    }
}

#[tokio::test]
#[serial]
async fn start_session_response_returns_within_500ms_even_when_subprocess_produces_output() {
    let tmp_home = std::env::temp_dir().join(format!(
        "omni-runs-noisy-home-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    std::fs::create_dir_all(&tmp_home).unwrap();
    unsafe {
        std::env::set_var("HOME", tmp_home.to_str().unwrap());
        std::env::set_var("OMNI_AGENT_CLAUDE_BIN", fixture("mock-agent-noisy.sh"));
    }

    let (app, state) = build_runs_app().await;
    let (project_id, task_id) = setup_assigned_task(&app, "claude").await;

    let start = std::time::Instant::now();
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/projects/{project_id}/tasks/{task_id}/sessions/start"
        ))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "{elapsed:?}"
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
    let log_path: String = sqlx::query_scalar("SELECT log_path FROM runs LIMIT 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(content.contains("stdout noisy 1"), "{content}");

    std::fs::remove_dir_all(&tmp_home).ok();
    unsafe {
        std::env::remove_var("HOME");
        std::env::remove_var("OMNI_AGENT_CLAUDE_BIN");
    }
}
