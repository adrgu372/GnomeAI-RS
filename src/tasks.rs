use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

use anyhow::{anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::storage::AppPaths;

static WORKFLOW_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const TASK_TERMINAL_STATUSES: &[&str] = &["completed", "failed", "killed", "cancelled"];
const TASK_STATUS_VALUES: &[&str] = &[
    "pending",
    "in_progress",
    "running",
    "completed",
    "failed",
    "killed",
    "blocked",
    "cancelled",
    "awaiting_input",
];
const TODO_STATUS_VALUES: &[&str] = &["pending", "in_progress", "completed"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct WorkflowStore {
    version: u32,
    scopes: BTreeMap<String, WorkflowScope>,
}

impl Default for WorkflowStore {
    fn default() -> Self {
        Self {
            version: 1,
            scopes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct WorkflowScope {
    todos: Vec<Value>,
    tasks: BTreeMap<String, Value>,
    task_order: Vec<String>,
    next_task_id: u64,
}

impl Default for WorkflowScope {
    fn default() -> Self {
        Self {
            todos: Vec::new(),
            tasks: BTreeMap::new(),
            task_order: Vec::new(),
            next_task_id: 1,
        }
    }
}

pub fn todo_write(paths: &AppPaths, scope_key: &str, raw_todos: &Value) -> anyhow::Result<Value> {
    let todos = sanitize_todos(raw_todos)?;
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let scope = ensure_scope(&mut store, scope_key);
    let old_todos = scope.todos.clone();
    let new_todos = if !todos.is_empty()
        && todos
            .iter()
            .all(|todo| todo.get("status").and_then(Value::as_str) == Some("completed"))
    {
        Vec::new()
    } else {
        todos
    };
    scope.todos = new_todos.clone();
    save_store(paths, &store)?;
    Ok(json!({"oldTodos": old_todos, "newTodos": new_todos, "scope": scope_key}))
}

pub fn task_create(
    paths: &AppPaths,
    scope_key: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let subject = required_text(args, "subject")?;
    let description = required_text(args, "description")?;
    let active_form = text_arg(args, "activeForm").unwrap_or_else(|| subject.clone());
    let metadata = sanitize_metadata(args.get("metadata"))?;

    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let scope = ensure_scope(&mut store, scope_key);
    let task_id = scope.next_task_id.max(1).to_string();
    scope.next_task_id = scope.next_task_id.max(1) + 1;

    let mut task = json!({
        "id": task_id,
        "type": "workflow_task",
        "subject": subject,
        "description": description,
        "activeForm": active_form,
        "status": "pending",
        "owner": "",
        "blocks": [],
        "blockedBy": [],
        "metadata": metadata,
        "createdAt": workflow_timestamp(),
        "updatedAt": workflow_timestamp(),
        "history": [],
        "outputFile": "",
    });
    let created_message = format!("Task created: {subject}");
    append_task_history(object_mut(&mut task)?, "created", &created_message, None);
    scope.tasks.insert(task_id.clone(), task);
    scope.task_order.push(task_id.clone());
    save_store(paths, &store)?;
    Ok(json!({"task": {"id": task_id, "subject": subject}, "scope": scope_key}))
}

pub fn create_runtime_task(
    paths: &AppPaths,
    scope_key: &str,
    task_type: &str,
    subject: &str,
    description: &str,
    prompt: &str,
    initial_status: &str,
    metadata: Value,
) -> anyhow::Result<Value> {
    let task_type = normalize_ws(task_type);
    let subject = normalize_ws(subject);
    let description = normalize_ws(description);
    let prompt = normalize_ws(prompt);
    let status = normalize_task_status(&json!(initial_status), false)?;
    let metadata = metadata
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, _)| !normalize_ws(key).is_empty())
        .collect::<Map<_, _>>();
    let agent_type = metadata
        .get("agent_type")
        .or_else(|| metadata.get("subagent_type"))
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    let model = metadata
        .get("model")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();

    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let scope = ensure_scope(&mut store, scope_key);
    let prefix = runtime_task_prefix(&task_type);
    let task_id = loop {
        let raw = Uuid::new_v4().simple().to_string();
        let candidate = format!("{prefix}{}", &raw[..8]);
        if !scope.tasks.contains_key(&candidate) {
            break candidate;
        }
    };
    let output_file = ensure_task_output_file(paths, &task_id)?;
    let active_form = if subject.is_empty() {
        task_id.clone()
    } else {
        subject.clone()
    };
    let mut task = json!({
        "id": task_id.as_str(),
        "type": if task_type.is_empty() { "runtime_task" } else { task_type.as_str() },
        "subject": if subject.is_empty() { task_id.as_str() } else { subject.as_str() },
        "description": description.as_str(),
        "activeForm": active_form.as_str(),
        "status": status.as_str(),
        "owner": "runtime",
        "blocks": [],
        "blockedBy": [],
        "metadata": metadata,
        "createdAt": workflow_timestamp(),
        "updatedAt": workflow_timestamp(),
        "history": [],
        "prompt": prompt.as_str(),
        "result": "",
        "error": "",
        "outputFile": output_file.to_string_lossy(),
        "agentType": agent_type.as_str(),
        "model": model.as_str(),
        "sessionUrl": "",
    });
    append_task_history(
        object_mut(&mut task)?,
        "created",
        &format!("Runtime task created: {}", active_form),
        None,
    );
    let public_task = task_public_view(&task, true)?;
    scope.tasks.insert(task_id.clone(), task);
    scope.task_order.push(task_id);
    save_store(paths, &store)?;
    Ok(public_task)
}

pub fn task_get(paths: &AppPaths, scope_key: &str, task_id: &str) -> anyhow::Result<Value> {
    if task_id.trim().is_empty() {
        bail!("taskId is required");
    }
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let store = load_store(paths);
    let task = store
        .scopes
        .get(scope_key)
        .and_then(|scope| scope.tasks.get(task_id))
        .cloned()
        .or_else(|| find_task_by_id_in_store(&store, task_id).map(|(_, task)| task));
    Ok(json!({
        "task": task.as_ref().and_then(|item| task_public_view(item, false).ok()),
        "scope": scope_key,
    }))
}

pub fn task_list(paths: &AppPaths, scope_key: &str) -> anyhow::Result<Value> {
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let scope = ensure_scope(&mut store, scope_key);
    let mut ordered_ids = scope
        .task_order
        .iter()
        .filter(|task_id| scope.tasks.contains_key(*task_id))
        .cloned()
        .collect::<Vec<_>>();
    for task_id in scope.tasks.keys() {
        if !ordered_ids.contains(task_id) {
            ordered_ids.push(task_id.clone());
        }
    }
    let tasks = ordered_ids
        .iter()
        .filter_map(|task_id| scope.tasks.get(task_id))
        .filter_map(|task| task_public_view(task, false).ok())
        .collect::<Vec<_>>();
    Ok(json!({"tasks": tasks, "scope": scope_key}))
}

/// Return the durable subagent registry. With no scope filter this is the
/// common WebTool/WhatsApp view; a chat scope can still request only its own
/// children when the model uses TaskList internally.
pub fn agent_list(paths: &AppPaths, scope_filter: Option<&str>) -> anyhow::Result<Value> {
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let store = load_store(paths);
    let mut agents = Vec::new();
    for (scope_key, scope) in &store.scopes {
        if scope_filter.is_some_and(|wanted| wanted != scope_key) {
            continue;
        }
        for task_id in &scope.task_order {
            let Some(task) = scope.tasks.get(task_id) else {
                continue;
            };
            if !is_agent_task(task) {
                continue;
            }
            let mut public = task_public_view(task, false)?;
            if let Some(obj) = public.as_object_mut() {
                obj.insert("scope".into(), json!(scope_key));
                obj.insert(
                    "outputPreview".into(),
                    json!(read_task_output_text(paths, task_id, 2_000)),
                );
            }
            agents.push(public);
        }
    }
    agents.sort_by(|left, right| {
        right
            .get("createdAt")
            .and_then(Value::as_str)
            .cmp(&left.get("createdAt").and_then(Value::as_str))
    });
    let running = agents
        .iter()
        .filter(|agent| {
            matches!(
                agent.get("status").and_then(Value::as_str),
                Some("pending" | "in_progress" | "running" | "awaiting_input")
            )
        })
        .count();
    let total = agents.len();
    Ok(json!({
        "agents": agents,
        "running": running,
        "total": total,
        "scope": scope_filter,
    }))
}

pub fn agent_get(paths: &AppPaths, agent_id: &str) -> anyhow::Result<Value> {
    if agent_id.trim().is_empty() {
        bail!("agent id is required");
    }
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let store = load_store(paths);
    let Some((scope, task)) = find_task_by_id_in_store(&store, agent_id) else {
        return Ok(json!({"agent": null}));
    };
    if !is_agent_task(&task) {
        return Ok(json!({"agent": null}));
    }
    let mut public = task_public_view(&task, true)?;
    if let Some(obj) = public.as_object_mut() {
        obj.insert("scope".into(), json!(scope));
        obj.insert(
            "output".into(),
            json!(read_task_output_text(paths, agent_id, 50_000)),
        );
    }
    Ok(json!({"agent": public}))
}

/// A process restart cannot keep Tokio workers alive. Preserve their history
/// but mark stale running subagents as failed so the shared registry never
/// claims that a vanished worker is still active.
pub fn recover_interrupted_agents(paths: &AppPaths) -> anyhow::Result<usize> {
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let mut recovered = 0usize;
    for scope in store.scopes.values_mut() {
        for task in scope.tasks.values_mut() {
            if !is_agent_task(task) {
                continue;
            }
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(
                status,
                "pending" | "in_progress" | "running" | "awaiting_input"
            ) {
                continue;
            }
            let obj = object_mut(task)?;
            obj.insert("status".into(), json!("failed"));
            obj.insert(
                "error".into(),
                json!("Subagent interrupted by application restart"),
            );
            append_task_history(
                obj,
                "failed",
                "Subagent interrupted by application restart",
                None,
            );
            recovered += 1;
        }
    }
    if recovered > 0 {
        save_store(paths, &store)?;
    }
    Ok(recovered)
}

pub fn task_update(
    paths: &AppPaths,
    scope_key: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let task_id = text_arg(args, "taskId")
        .or_else(|| text_arg(args, "task_id"))
        .ok_or_else(|| anyhow!("taskId is required"))?;

    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let scope = ensure_scope(&mut store, scope_key);
    if !scope.tasks.contains_key(&task_id) {
        return Ok(json!({
            "success": false,
            "taskId": task_id,
            "updatedFields": [],
            "error": "Task not found",
            "scope": scope_key,
        }));
    }

    if args
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| normalize_ws(status) == "deleted")
    {
        let previous_status = scope
            .tasks
            .get(&task_id)
            .and_then(|task| task.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();
        scope.tasks.remove(&task_id);
        scope.task_order.retain(|item| item != &task_id);
        for task in scope.tasks.values_mut() {
            scrub_dependency(task, "blocks", &task_id);
            scrub_dependency(task, "blockedBy", &task_id);
        }
        save_store(paths, &store)?;
        return Ok(json!({
            "success": true,
            "taskId": task_id,
            "updatedFields": ["deleted"],
            "statusChange": {"from": previous_status, "to": "deleted"},
            "scope": scope_key,
        }));
    }

    let task = scope.tasks.get_mut(&task_id).unwrap();
    let task_obj = object_mut(task)?;
    let mut updated_fields = Vec::new();
    let mut changes = Map::new();
    let mut status_change = Value::Null;

    if let Some(status_value) = args.get("status") {
        let status = normalize_task_status(status_value, false)?;
        let old = task_obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();
        if status != old {
            task_obj.insert("status".into(), json!(status));
            updated_fields.push("status".to_string());
            status_change = json!({"from": old, "to": status});
            changes.insert("status".into(), status_change.clone());
        }
    }

    for (field, arg_name) in [
        ("subject", "subject"),
        ("description", "description"),
        ("activeForm", "activeForm"),
    ] {
        if let Some(new_value) = text_arg(args, arg_name) {
            if task_obj.get(field).and_then(Value::as_str) != Some(new_value.as_str()) {
                task_obj.insert(field.into(), json!(new_value));
                updated_fields.push(field.to_string());
                changes.insert(
                    field.into(),
                    task_obj.get(field).cloned().unwrap_or(Value::Null),
                );
            }
        }
    }

    if args.contains_key("owner") {
        let owner = text_arg(args, "owner").unwrap_or_default();
        if task_obj.get("owner").and_then(Value::as_str).unwrap_or("") != owner {
            task_obj.insert("owner".into(), json!(owner));
            updated_fields.push("owner".into());
            changes.insert(
                "owner".into(),
                task_obj.get("owner").cloned().unwrap_or(Value::Null),
            );
        }
    }

    if let Some(metadata) = args.get("metadata") {
        let incoming = metadata
            .as_object()
            .ok_or_else(|| anyhow!("metadata must be an object"))?;
        let mut merged = task_obj
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, value) in incoming {
            let name = normalize_ws(key);
            if name.is_empty() {
                continue;
            }
            if value.is_null() {
                merged.remove(&name);
            } else {
                merged.insert(name, value.clone());
            }
        }
        task_obj.insert("metadata".into(), Value::Object(merged.clone()));
        updated_fields.push("metadata".into());
        changes.insert("metadata".into(), Value::Object(merged));
    }

    if let Some(add_blocks) = args.get("addBlocks") {
        let merged = merge_string_list(task_obj.get("blocks"), add_blocks, &task_id);
        task_obj.insert("blocks".into(), json!(merged));
        updated_fields.push("blocks".into());
        changes.insert(
            "blocks".into(),
            task_obj.get("blocks").cloned().unwrap_or(Value::Null),
        );
    }
    if let Some(add_blocked_by) = args.get("addBlockedBy") {
        let merged = merge_string_list(task_obj.get("blockedBy"), add_blocked_by, &task_id);
        task_obj.insert("blockedBy".into(), json!(merged));
        updated_fields.push("blockedBy".into());
        changes.insert(
            "blockedBy".into(),
            task_obj.get("blockedBy").cloned().unwrap_or(Value::Null),
        );
    }

    if !updated_fields.is_empty() {
        append_task_history(
            task_obj,
            "updated",
            &format!("Task updated: {}", updated_fields.join(", ")),
            Some(Value::Object(changes)),
        );
        save_store(paths, &store)?;
    }

    Ok(json!({
        "success": true,
        "taskId": task_id,
        "updatedFields": updated_fields,
        "statusChange": status_change,
        "scope": scope_key,
    }))
}

pub fn task_stop(paths: &AppPaths, scope_key: &str, task_id: &str) -> anyhow::Result<Value> {
    if task_id.trim().is_empty() {
        bail!("task_id is required");
    }
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let (found_scope, task_type, command) = {
        let Some((found_scope, task)) = find_task_mut(&mut store, scope_key, task_id) else {
            bail!("No task found with ID: {task_id}");
        };
        let task_type = task
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("workflow_task")
            .to_string();
        let status = if task_type == "workflow_task" {
            "cancelled"
        } else {
            "killed"
        };
        let obj = object_mut(task)?;
        let command = obj
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or(task_id)
            .to_string();
        obj.insert("status".into(), json!(status));
        append_task_history(obj, "stopped", &format!("Task stopped: {command}"), None);
        (found_scope, task_type, command)
    };
    save_store(paths, &store)?;
    Ok(json!({
        "message": format!("Task {task_id} was stopped"),
        "task_id": task_id,
        "task_type": task_type,
        "command": command,
        "scope": found_scope,
    }))
}

pub fn task_output(
    paths: &AppPaths,
    scope_key: &str,
    task_id: &str,
    block: bool,
) -> anyhow::Result<Value> {
    if task_id.trim().is_empty() {
        bail!("task_id is required");
    }
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let store = load_store(paths);
    let Some((found_scope, task)) = find_task_by_id_in_store(&store, task_id) else {
        return Ok(json!({"retrieval_status": "not_found", "task": null, "scope": scope_key}));
    };
    let public_task = task_public_view(&task, true)?;
    let output_file = public_task
        .get("outputFile")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output = if !output_file.is_empty() {
        read_task_output_text(paths, task_id, 20_000)
    } else {
        public_task
            .get("history")
            .and_then(Value::as_array)
            .map(|history| {
                history
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|entry| {
                        format!(
                            "{} [{}] {}",
                            entry.get("timestamp").and_then(Value::as_str).unwrap_or(""),
                            entry.get("type").and_then(Value::as_str).unwrap_or("event"),
                            entry.get("message").and_then(Value::as_str).unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    let status = public_task
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let retrieval_status = if TASK_TERMINAL_STATUSES.contains(&status) {
        "success"
    } else if block {
        "timeout"
    } else {
        "not_ready"
    };
    Ok(json!({
        "retrieval_status": retrieval_status,
        "task": {
            "task_id": public_task.get("id").cloned().unwrap_or(Value::Null),
            "task_type": public_task.get("type").cloned().unwrap_or_else(|| json!("workflow_task")),
            "status": public_task.get("status").cloned().unwrap_or(Value::Null),
            "description": public_task.get("description").cloned().unwrap_or(Value::Null),
            "output": output,
            "result": public_task.get("result").cloned().or_else(|| public_task.get("subject").cloned()).unwrap_or(Value::Null),
            "history": public_task.get("history").cloned().unwrap_or_else(|| json!([])),
            "outputFile": public_task.get("outputFile").cloned().unwrap_or(Value::Null),
            "error": public_task.get("error").cloned().unwrap_or(Value::Null),
        },
        "scope": found_scope,
    }))
}

pub fn runtime_task_update(paths: &AppPaths, task_id: &str, data: &Value) -> anyhow::Result<Value> {
    let Some(data) = data.as_object() else {
        bail!("payload must be an object");
    };
    if !find_task_by_id(paths, task_id)?.is_some() {
        bail!("Task not found");
    }
    if let Some(text) = data.get("append_output").and_then(Value::as_str) {
        append_task_output_text(paths, task_id, text)?;
    }
    if let Some(text) = data.get("output").and_then(Value::as_str) {
        append_task_output_text(paths, task_id, text)?;
    }

    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let Some((_scope, task)) = find_task_mut_any(&mut store, task_id) else {
        bail!("Task not found");
    };
    let obj = object_mut(task)?;
    if let Some(session_url) = data
        .get("session_url")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .filter(|item| !item.is_empty())
    {
        obj.insert("sessionUrl".into(), json!(session_url));
    }
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    if ["pending", "running", "awaiting_input"].contains(&status.as_str()) {
        obj.insert("status".into(), json!(status));
        append_task_history(
            obj,
            &status,
            data.get("message")
                .and_then(Value::as_str)
                .map(normalize_ws)
                .filter(|item| !item.is_empty())
                .unwrap_or_else(|| format!("Task {status}"))
                .as_str(),
            None,
        );
    } else if ["completed", "failed", "killed"].contains(&status.as_str()) {
        obj.insert("status".into(), json!(status));
        if let Some(result) = data.get("result").and_then(Value::as_str) {
            obj.insert("result".into(), json!(result));
        }
        if let Some(error) = data.get("error").and_then(Value::as_str) {
            obj.insert("error".into(), json!(error));
        }
        let mut changes = Map::new();
        if let Some(result) = data.get("result") {
            changes.insert("result".into(), result.clone());
        }
        if let Some(error) = data.get("error") {
            changes.insert("error".into(), error.clone());
        }
        append_task_history(
            obj,
            &status,
            &format!("Task {status}"),
            (!changes.is_empty()).then_some(Value::Object(changes)),
        );
    }
    save_store(paths, &store)?;
    Ok(json!({"ok": true}))
}

pub fn mark_task_terminal(
    paths: &AppPaths,
    task_id: &str,
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
) -> anyhow::Result<Option<Value>> {
    let status = normalize_ws(status);
    if !TASK_TERMINAL_STATUSES.contains(&status.as_str()) {
        bail!("status must be terminal");
    }

    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let mut store = load_store(paths);
    let Some((_scope, task)) = find_task_mut_any(&mut store, task_id) else {
        return Ok(None);
    };

    let current_status = task
        .get("status")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    if TASK_TERMINAL_STATUSES.contains(&current_status.as_str()) {
        return task_public_view(task, true).map(Some);
    }

    {
        let obj = object_mut(task)?;
        obj.insert("status".into(), json!(status));
        let mut changes = Map::new();
        if let Some(result) = result.map(normalize_ws).filter(|item| !item.is_empty()) {
            obj.insert("result".into(), json!(result));
            changes.insert(
                "result".into(),
                obj.get("result").cloned().unwrap_or(Value::Null),
            );
        }
        if let Some(error) = error.map(normalize_ws).filter(|item| !item.is_empty()) {
            obj.insert("error".into(), json!(error));
            changes.insert(
                "error".into(),
                obj.get("error").cloned().unwrap_or(Value::Null),
            );
        }
        append_task_history(
            obj,
            &status,
            &format!("Task {status}"),
            (!changes.is_empty()).then_some(Value::Object(changes)),
        );
    }
    let public_task = task_public_view(task, true)?;
    save_store(paths, &store)?;
    Ok(Some(public_task))
}

pub fn find_task_by_id(paths: &AppPaths, task_id: &str) -> anyhow::Result<Option<(String, Value)>> {
    let _guard = WORKFLOW_LOCK.lock().unwrap();
    let store = load_store(paths);
    Ok(find_task_by_id_in_store(&store, task_id))
}

fn load_store(paths: &AppPaths) -> WorkflowStore {
    if !paths.workflow_file.exists() {
        return WorkflowStore::default();
    }
    fs::read_to_string(&paths.workflow_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<WorkflowStore>(&raw).ok())
        .unwrap_or_default()
}

fn save_store(paths: &AppPaths, store: &WorkflowStore) -> anyhow::Result<()> {
    if let Some(parent) = paths.workflow_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&paths.workflow_file, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

fn ensure_scope<'a>(store: &'a mut WorkflowStore, scope_key: &str) -> &'a mut WorkflowScope {
    let scope = store.scopes.entry(scope_key.to_string()).or_default();
    if scope.next_task_id == 0 {
        scope.next_task_id = 1;
    }
    scope
}

fn task_public_view(task: &Value, include_history: bool) -> anyhow::Result<Value> {
    let obj = task
        .as_object()
        .ok_or_else(|| anyhow!("task must be an object"))?;
    let mut data = Map::new();
    for (key, fallback) in [
        ("id", ""),
        ("subject", ""),
        ("description", ""),
        ("activeForm", ""),
        ("status", "pending"),
        ("createdAt", ""),
        ("updatedAt", ""),
        ("type", "workflow_task"),
        ("outputFile", ""),
    ] {
        data.insert(
            key.into(),
            json!(obj.get(key).and_then(Value::as_str).unwrap_or(fallback)),
        );
    }
    let owner = obj
        .get("owner")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    data.insert(
        "owner".into(),
        if owner.is_empty() {
            Value::Null
        } else {
            json!(owner)
        },
    );
    data.insert(
        "blocks".into(),
        json!(sanitize_string_list(obj.get("blocks"))),
    );
    data.insert(
        "blockedBy".into(),
        json!(sanitize_string_list(obj.get("blockedBy"))),
    );
    data.insert(
        "metadata".into(),
        obj.get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({})),
    );
    if include_history {
        data.insert(
            "history".into(),
            obj.get("history").cloned().unwrap_or_else(|| json!([])),
        );
    }
    for optional_key in [
        "prompt",
        "result",
        "error",
        "sessionUrl",
        "agentType",
        "model",
        "questionId",
    ] {
        if let Some(value) = obj
            .get(optional_key)
            .filter(|value| !value.is_null() && *value != "")
        {
            data.insert(optional_key.into(), value.clone());
        }
    }
    Ok(Value::Object(data))
}

fn append_task_history(
    task: &mut Map<String, Value>,
    event_type: &str,
    message: &str,
    changes: Option<Value>,
) {
    let timestamp = workflow_timestamp();
    let mut entry = json!({
        "timestamp": timestamp,
        "type": event_type,
        "message": message,
    });
    if let Some(changes) = changes {
        entry["changes"] = changes;
    }
    if !task.get("history").is_some_and(Value::is_array) {
        task.insert("history".into(), json!([]));
    }
    if let Some(history) = task.get_mut("history").and_then(Value::as_array_mut) {
        history.push(entry);
    }
    task.insert("updatedAt".into(), json!(timestamp));
}

fn find_task_by_id_in_store(store: &WorkflowStore, task_id: &str) -> Option<(String, Value)> {
    for (scope_key, scope) in &store.scopes {
        if let Some(task) = scope.tasks.get(task_id) {
            return Some((scope_key.clone(), task.clone()));
        }
    }
    None
}

fn find_task_mut<'a>(
    store: &'a mut WorkflowStore,
    scope_key: &str,
    task_id: &str,
) -> Option<(String, &'a mut Value)> {
    if store
        .scopes
        .get(scope_key)
        .is_some_and(|scope| scope.tasks.contains_key(task_id))
    {
        let task = store.scopes.get_mut(scope_key)?.tasks.get_mut(task_id)?;
        return Some((scope_key.to_string(), task));
    }
    find_task_mut_any(store, task_id)
}

fn find_task_mut_any<'a>(
    store: &'a mut WorkflowStore,
    task_id: &str,
) -> Option<(String, &'a mut Value)> {
    for (scope_key, scope) in &mut store.scopes {
        if let Some(task) = scope.tasks.get_mut(task_id) {
            return Some((scope_key.clone(), task));
        }
    }
    None
}

fn sanitize_todos(raw_todos: &Value) -> anyhow::Result<Vec<Value>> {
    let items = raw_todos
        .as_array()
        .ok_or_else(|| anyhow!("todos must be a list"))?;
    let mut todos = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| anyhow!("todos[{}] must be an object", idx + 1))?;
        let content = obj
            .get("content")
            .and_then(Value::as_str)
            .map(normalize_ws)
            .unwrap_or_default();
        let active_form = obj
            .get("activeForm")
            .or_else(|| obj.get("active_form"))
            .and_then(Value::as_str)
            .map(normalize_ws)
            .filter(|item| !item.is_empty())
            .unwrap_or_else(|| content.clone());
        let status = obj
            .get("status")
            .and_then(Value::as_str)
            .map(normalize_ws)
            .unwrap_or_default();
        if content.is_empty() {
            bail!("todos[{}].content is required", idx + 1);
        }
        if !TODO_STATUS_VALUES.contains(&status.as_str()) {
            bail!(
                "todos[{}].status must be one of: {}",
                idx + 1,
                TODO_STATUS_VALUES.join(", ")
            );
        }
        todos.push(json!({"content": content, "status": status, "activeForm": active_form}));
    }
    Ok(todos)
}

fn sanitize_metadata(raw: Option<&Value>) -> anyhow::Result<Map<String, Value>> {
    let Some(raw) = raw else {
        return Ok(Map::new());
    };
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow!("metadata must be an object"))?;
    let mut cleaned = Map::new();
    for (key, value) in obj {
        let name = normalize_ws(key);
        if !name.is_empty() {
            cleaned.insert(name, value.clone());
        }
    }
    Ok(cleaned)
}

fn normalize_task_status(value: &Value, allow_deleted: bool) -> anyhow::Result<String> {
    let status = value.as_str().map(normalize_ws).unwrap_or_default();
    if TASK_STATUS_VALUES.contains(&status.as_str()) || (allow_deleted && status == "deleted") {
        Ok(status)
    } else {
        bail!("status must be one of: {}", TASK_STATUS_VALUES.join(", "))
    }
}

fn runtime_task_prefix(task_type: &str) -> &'static str {
    match task_type {
        "local_bash" => "b",
        "local_agent" => "a",
        "remote_agent" => "r",
        _ => "w",
    }
}

fn is_agent_task(task: &Value) -> bool {
    matches!(
        task.get("type").and_then(Value::as_str),
        Some("local_agent" | "remote_agent")
    )
}

fn sanitize_string_list(value: Option<&Value>) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let text = item.as_str().map(normalize_ws).unwrap_or_default();
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
    }
    out
}

fn merge_string_list(current: Option<&Value>, incoming: &Value, own_id: &str) -> Vec<String> {
    let mut merged = sanitize_string_list(current);
    for item in sanitize_string_list(Some(incoming)) {
        if item != own_id && !merged.contains(&item) {
            merged.push(item);
        }
    }
    merged
}

fn scrub_dependency(task: &mut Value, field: &str, task_id: &str) {
    if let Some(obj) = task.as_object_mut() {
        let cleaned = sanitize_string_list(obj.get(field))
            .into_iter()
            .filter(|item| item != task_id)
            .collect::<Vec<_>>();
        obj.insert(field.into(), json!(cleaned));
    }
}

fn required_text(args: &Map<String, Value>, key: &str) -> anyhow::Result<String> {
    text_arg(args, key)
        .filter(|item| !item.is_empty())
        .ok_or_else(|| anyhow!("{key} is required"))
}

fn text_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key).and_then(|value| match value {
        Value::String(text) => Some(normalize_ws(text)),
        Value::Null => None,
        other => Some(normalize_ws(&other.to_string())),
    })
}

fn object_mut(value: &mut Value) -> anyhow::Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("task must be an object"))
}

fn workflow_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ensure_task_output_file(paths: &AppPaths, task_id: &str) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&paths.task_outputs_dir)?;
    let path = paths.task_outputs_dir.join(format!("{task_id}.log"));
    if !path.exists() {
        fs::write(&path, "")?;
    }
    Ok(path)
}

pub fn append_task_output_text(paths: &AppPaths, task_id: &str, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let path = ensure_task_output_file(paths, task_id)?;
    let mut current = fs::read_to_string(&path).unwrap_or_default();
    current.push_str(text);
    fs::write(path, current)?;
    Ok(())
}

fn read_task_output_text(paths: &AppPaths, task_id: &str, max_chars: usize) -> String {
    let path = paths.task_outputs_dir.join(format!("{task_id}.log"));
    let raw = fs::read_to_string(path).unwrap_or_default();
    let chars = raw.chars().collect::<Vec<_>>();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

fn normalize_ws(text: &str) -> String {
    regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(text, " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(label: &str) -> (PathBuf, AppPaths) {
        let root =
            std::env::temp_dir().join(format!("gnomef-rs-{label}-{}", Uuid::new_v4().simple()));
        let paths = AppPaths::new(root.clone()).unwrap();
        (root, paths)
    }

    #[test]
    fn agent_registry_is_shared_across_web_and_whatsapp_scopes() {
        let (root, paths) = test_paths("agent-registry");
        let web = create_runtime_task(
            &paths,
            "chat-1",
            "local_agent",
            "Explore code",
            "Explore code",
            "Find the parser",
            "running",
            json!({"agent_type": "Explore", "provider_id": "custom"}),
        )
        .unwrap();
        let whatsapp = create_runtime_task(
            &paths,
            "whatsapp_40700",
            "local_agent",
            "Implement fix",
            "Implement fix",
            "Patch the parser",
            "completed",
            json!({"agent_type": "general-purpose", "provider_id": "deepseek"}),
        )
        .unwrap();
        append_task_output_text(
            &paths,
            whatsapp.get("id").and_then(Value::as_str).unwrap(),
            "done",
        )
        .unwrap();

        let registry = agent_list(&paths, None).unwrap();
        assert_eq!(registry["total"], 2);
        assert_eq!(registry["running"], 1);
        let agents = registry["agents"].as_array().unwrap();
        assert!(agents.iter().any(|agent| agent["scope"] == "chat-1"));
        assert!(
            agents
                .iter()
                .any(|agent| agent["scope"] == "whatsapp_40700")
        );
        let detail =
            agent_get(&paths, whatsapp.get("id").and_then(Value::as_str).unwrap()).unwrap();
        assert!(detail["agent"]["output"].as_str().unwrap().contains("done"));
        assert!(web.get("id").is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restart_marks_only_live_agents_as_interrupted() {
        let (root, paths) = test_paths("agent-recovery");
        let running = create_runtime_task(
            &paths,
            "chat",
            "local_agent",
            "Running",
            "Running",
            "work",
            "running",
            json!({}),
        )
        .unwrap();
        let completed = create_runtime_task(
            &paths,
            "chat",
            "local_agent",
            "Completed",
            "Completed",
            "work",
            "completed",
            json!({}),
        )
        .unwrap();
        assert_eq!(recover_interrupted_agents(&paths).unwrap(), 1);
        assert_eq!(
            agent_get(&paths, running.get("id").and_then(Value::as_str).unwrap()).unwrap()["agent"]
                ["status"],
            "failed"
        );
        assert_eq!(
            agent_get(&paths, completed.get("id").and_then(Value::as_str).unwrap()).unwrap()["agent"]
                ["status"],
            "completed"
        );
        let _ = fs::remove_dir_all(root);
    }
}
