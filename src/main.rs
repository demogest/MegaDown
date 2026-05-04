#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use megadown::{
    cancel_download, choose_download_dir, clear_finished_tasks, default_download_dir, delete_task,
    inspect_link, list_tasks, open_task_file, open_task_folder, pause_all_downloads,
    pause_download, resume_all_downloads, resume_download, start_download, AppState,
    DownloadRequest, PerformanceMode, RetryMode, TaskSnapshot, TaskStatus,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

slint::include_modules!();

const CONNECTION_OPTIONS: [usize; 7] = [2, 4, 8, 12, 16, 24, 32];
const CHUNK_OPTIONS_MB: [u64; 5] = [2, 4, 8, 16, 32];

fn main() -> Result<(), slint::PlatformError> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create Tokio runtime"),
    );
    let state = AppState::default();
    let selected_id = Arc::new(Mutex::new(None::<String>));
    let running = Arc::new(AtomicBool::new(true));

    let ui = MegaDownWindow::new()?;
    ui.set_output_dir(default_download_dir().unwrap_or_default().into());
    ui.set_notice_title("".into());
    ui.set_notice_text("".into());

    refresh_ui(&ui, &state, &selected_id);
    wire_callbacks(&ui, state.clone(), selected_id.clone(), runtime.clone());
    start_refresh_loop(
        ui.as_weak(),
        state.clone(),
        selected_id.clone(),
        running.clone(),
    );

    let result = ui.run();
    running.store(false, Ordering::Relaxed);
    drop(runtime);
    result
}

fn wire_callbacks(
    ui: &MegaDownWindow,
    state: AppState,
    selected_id: Arc<Mutex<Option<String>>>,
    runtime: Arc<tokio::runtime::Runtime>,
) {
    ui.on_filter_requested({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_filter_index(index);
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });

    ui.on_select_task({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move |id| {
            *selected_id.lock().expect("selected task lock poisoned") = Some(id.to_string());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_active_tab(1);
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });

    ui.on_choose_dir_requested({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        move || {
            let ui_async = ui_weak.clone();
            runtime.spawn(async move {
                match choose_download_dir().await {
                    Ok(Some(path)) => {
                        let _ = ui_async.upgrade_in_event_loop(move |ui| {
                            ui.set_output_dir(path.into());
                        });
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = ui_async.upgrade_in_event_loop(move |ui| {
                            set_notice(&ui, "选择目录失败", err, true);
                        });
                    }
                }
            });
        }
    });

    ui.on_inspect_requested({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let links = extract_mega_links(&ui.get_url_text());
            let Some(url) = links.first().cloned() else {
                set_notice(&ui, "缺少链接", "请输入 MEGA 公共链接。", true);
                return;
            };
            let password = non_empty(ui.get_password_text().as_str());
            ui.set_busy(true);

            let ui_async = ui_weak.clone();
            runtime.spawn(async move {
                let result = inspect_link(url, password).await;
                let _ = ui_async.upgrade_in_event_loop(move |ui| {
                    ui.set_busy(false);
                    match result {
                        Ok(info) if info.requires_password => {
                            set_notice(&ui, "需要密码", "请输入共享密码后再读取。", true);
                        }
                        Ok(info) => {
                            let kind = if info.kind == "folder" {
                                "文件夹"
                            } else {
                                "文件"
                            };
                            set_notice(
                                &ui,
                                format!("{kind}: {}", info.name),
                                format!(
                                    "{} · {} 个文件",
                                    format_bytes(info.total_bytes),
                                    info.file_count
                                ),
                                false,
                            );
                        }
                        Err(err) => set_notice(&ui, "链接解析失败", err, true),
                    }
                });
            });
        }
    });

    ui.on_start_requested({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        let runtime = runtime.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let links = extract_mega_links(&ui.get_url_text());
            if links.is_empty() {
                set_notice(&ui, "缺少链接", "请输入 MEGA 公共链接。", true);
                return;
            }

            let request_settings = RequestSettings::from_ui(&ui);
            ui.set_busy(true);
            let ui_async = ui_weak.clone();
            let state_async = state.clone();
            let selected_async = selected_id.clone();

            runtime.spawn(async move {
                let mut added = 0usize;
                let mut failed = Vec::new();
                let mut last_task_id = None;

                for url in links {
                    let request = request_settings.download_request(url.clone());
                    match start_download(state_async.clone(), request).await {
                        Ok(task) => {
                            added += 1;
                            last_task_id = Some(task.id);
                        }
                        Err(err) => failed.push((url, err)),
                    }
                }

                if let Some(id) = last_task_id {
                    *selected_async.lock().expect("selected task lock poisoned") = Some(id);
                }

                let _ = ui_async.upgrade_in_event_loop(move |ui| {
                    ui.set_busy(false);
                    if failed.is_empty() {
                        ui.set_url_text("".into());
                        ui.set_password_text("".into());
                        set_notice(
                            &ui,
                            "已加入队列",
                            format!("{added} 个链接已开始下载。"),
                            false,
                        );
                    } else {
                        ui.set_url_text(
                            failed
                                .iter()
                                .map(|(url, _)| url.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                                .into(),
                        );
                        let title = if added > 0 {
                            format!("已添加 {added} 个，失败 {} 个", failed.len())
                        } else {
                            "无法开始下载".to_string()
                        };
                        set_notice(&ui, title, format!("首个错误：{}", failed[0].1), added == 0);
                    }
                    refresh_ui(&ui, &state_async, &selected_async);
                });
            });
        }
    });

    ui.on_pause_task(sync_task_action(
        ui,
        state.clone(),
        selected_id.clone(),
        pause_download,
        "暂停失败",
    ));
    ui.on_resume_task(sync_task_action(
        ui,
        state.clone(),
        selected_id.clone(),
        resume_download,
        "继续失败",
    ));
    ui.on_cancel_task(sync_task_action(
        ui,
        state.clone(),
        selected_id.clone(),
        cancel_download,
        "取消失败",
    ));
    ui.on_open_task_file(sync_task_action(
        ui,
        state.clone(),
        selected_id.clone(),
        open_task_file,
        "打开文件失败",
    ));
    ui.on_open_task_folder(sync_task_action(
        ui,
        state.clone(),
        selected_id.clone(),
        open_task_folder,
        "打开目录失败",
    ));

    ui.on_delete_task({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move |id| {
            let id = id.to_string();
            if let Err(err) = delete_task(&state, &id) {
                if let Some(ui) = ui_weak.upgrade() {
                    set_notice(&ui, "删除任务失败", err, true);
                }
            }
            if selected_id
                .lock()
                .expect("selected task lock poisoned")
                .as_deref()
                == Some(id.as_str())
            {
                *selected_id.lock().expect("selected task lock poisoned") = None;
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });

    ui.on_retry_task({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        let runtime = runtime.clone();
        move |id| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let id = id.to_string();
            let Some(task) = list_tasks(&state).into_iter().find(|task| task.id == id) else {
                set_notice(&ui, "无法重试", "任务不存在。", true);
                return;
            };
            if task.url.trim().is_empty() {
                set_notice(&ui, "无法重试", "任务缺少原始链接。", true);
                return;
            }

            let mut request_settings = RequestSettings::from_ui(&ui);
            request_settings.output_dir = Some(non_empty_or(task.output_dir.clone()));
            request_settings.connections = task.connections;
            request_settings.chunk_size_mb = chunk_mb_from_bytes(task.chunk_size_bytes);
            request_settings.performance_mode = task.performance_mode;
            request_settings.retry_mode = task.retry_mode;
            request_settings.overwrite =
                task.overwrite || matches!(task.status, TaskStatus::Completed);
            request_settings.verify_integrity = task.verify_integrity;
            ui.set_busy(true);

            let ui_async = ui_weak.clone();
            let state_async = state.clone();
            let selected_async = selected_id.clone();
            runtime.spawn(async move {
                let result = start_download(
                    state_async.clone(),
                    request_settings.download_request(task.url),
                )
                .await;
                let _ = ui_async.upgrade_in_event_loop(move |ui| {
                    ui.set_busy(false);
                    match result {
                        Ok(task) => {
                            *selected_async.lock().expect("selected task lock poisoned") =
                                Some(task.id);
                            set_notice(&ui, "已重新加入队列", "任务已按原参数处理。", false);
                        }
                        Err(err) => set_notice(&ui, "重试失败", err, true),
                    }
                    refresh_ui(&ui, &state_async, &selected_async);
                });
            });
        }
    });

    ui.on_pause_all_requested({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move || {
            if let Err(err) = pause_all_downloads(&state) {
                if let Some(ui) = ui_weak.upgrade() {
                    set_notice(&ui, "全部暂停失败", err, true);
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });

    ui.on_resume_all_requested({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move || {
            if let Err(err) = resume_all_downloads(&state) {
                if let Some(ui) = ui_weak.upgrade() {
                    set_notice(&ui, "全部继续失败", err, true);
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });

    ui.on_clear_finished_requested({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        let selected_id = selected_id.clone();
        move || {
            match clear_finished_tasks(&state) {
                Ok(removed) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        let title = if removed > 0 {
                            "已清空任务"
                        } else {
                            "没有可清空任务"
                        };
                        let text = if removed > 0 {
                            format!("已移除 {removed} 个已结束任务记录。")
                        } else {
                            "运行中任务会保留在队列中。".to_string()
                        };
                        set_notice(&ui, title, text, false);
                    }
                }
                Err(err) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        set_notice(&ui, "清空任务失败", err, true);
                    }
                }
            }
            *selected_id.lock().expect("selected task lock poisoned") = None;
            if let Some(ui) = ui_weak.upgrade() {
                refresh_ui(&ui, &state, &selected_id);
            }
        }
    });
}

fn sync_task_action(
    ui: &MegaDownWindow,
    state: AppState,
    selected_id: Arc<Mutex<Option<String>>>,
    action: fn(&AppState, &str) -> Result<(), String>,
    error_title: &'static str,
) -> impl Fn(SharedString) + 'static {
    let ui_weak = ui.as_weak();
    move |id| {
        let id = id.to_string();
        if let Err(err) = action(&state, &id) {
            if let Some(ui) = ui_weak.upgrade() {
                set_notice(&ui, error_title, err, true);
            }
        }
        if let Some(ui) = ui_weak.upgrade() {
            refresh_ui(&ui, &state, &selected_id);
        }
    }
}

fn start_refresh_loop(
    ui_weak: slint::Weak<MegaDownWindow>,
    state: AppState,
    selected_id: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(900));
            let state = state.clone();
            let selected_id = selected_id.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                refresh_ui(&ui, &state, &selected_id);
            });
        }
    });
}

fn refresh_ui(ui: &MegaDownWindow, state: &AppState, selected_id: &Arc<Mutex<Option<String>>>) {
    let all_tasks = list_tasks(state);
    let filter_index = ui.get_filter_index();
    let filtered: Vec<_> = all_tasks
        .iter()
        .filter(|task| task_matches_filter(task, filter_index))
        .cloned()
        .collect();

    let selected = resolve_selected_task(&all_tasks, &filtered, selected_id);
    let ui_tasks: Vec<UiTask> = filtered.iter().map(task_to_ui).collect();
    ui.set_tasks(ModelRc::from(Rc::new(VecModel::from(ui_tasks))));

    if let Some(task) = selected {
        ui.set_selected_task(task_to_ui(&task));
        ui.set_has_selected_task(true);
    } else {
        ui.set_selected_task(UiTask::default());
        ui.set_has_selected_task(false);
    }

    let all_count = all_tasks.len();
    let active_count = all_tasks
        .iter()
        .filter(|task| is_active(&task.status))
        .count();
    let paused_count = all_tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Paused))
        .count();
    let completed_count = all_tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Completed))
        .count();
    let failed_count = all_tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Failed))
        .count();
    let cancelled_count = all_tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Cancelled))
        .count();
    let total_speed = all_tasks
        .iter()
        .filter(|task| is_active(&task.status))
        .map(|task| task.speed_bps)
        .sum::<f64>();

    ui.set_all_count(all_count.to_string().into());
    ui.set_active_count(active_count.to_string().into());
    ui.set_paused_count(paused_count.to_string().into());
    ui.set_completed_count(completed_count.to_string().into());
    ui.set_failed_count(failed_count.to_string().into());
    ui.set_cancelled_count(cancelled_count.to_string().into());
    ui.set_total_speed_text(format!("{}/s", format_bytes(total_speed as u64)).into());
    ui.set_visible_count_text(format!("{} / {} 个任务", filtered.len(), all_count).into());
    ui.set_has_pausable(all_tasks.iter().any(|task| can_pause(&task.status)));
    ui.set_has_paused(
        all_tasks
            .iter()
            .any(|task| matches!(task.status, TaskStatus::Paused)),
    );
    ui.set_has_clearable(all_tasks.iter().any(|task| is_terminal(&task.status)));
}

fn resolve_selected_task(
    all_tasks: &[TaskSnapshot],
    filtered: &[TaskSnapshot],
    selected_id: &Arc<Mutex<Option<String>>>,
) -> Option<TaskSnapshot> {
    let mut guard = selected_id.lock().expect("selected task lock poisoned");
    if let Some(id) = guard.as_deref() {
        if let Some(task) = all_tasks.iter().find(|task| task.id == id) {
            return Some(task.clone());
        }
    }

    let fallback = filtered.last().or_else(|| all_tasks.last()).cloned();
    *guard = fallback.as_ref().map(|task| task.id.clone());
    fallback
}

fn task_to_ui(task: &TaskSnapshot) -> UiTask {
    let total_bytes = task.total_bytes;
    let downloaded_bytes = task.downloaded_bytes;
    let progress_value = if total_bytes > 0 {
        ((downloaded_bytes as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0)
    } else if matches!(task.status, TaskStatus::Completed) {
        100.0
    } else {
        0.0
    };
    let remaining = total_bytes.saturating_sub(downloaded_bytes);
    let detail = task
        .error
        .clone()
        .or_else(|| task.current_file.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| task.output_path.clone());

    UiTask {
        id: task.id.clone().into(),
        file_name: task.file_name.clone().into(),
        detail: non_empty_or(detail).into(),
        status_text: status_text(&task.status).into(),
        status_kind: status_kind(&task.status).into(),
        downloaded_text: format!(
            "{} / {}",
            format_bytes(downloaded_bytes),
            format_bytes(total_bytes)
        )
        .into(),
        speed_text: format!("{}/s", format_bytes(task.speed_bps as u64)).into(),
        eta_text: format_eta(&task.status, remaining, task.speed_bps as u64).into(),
        config_text: format!(
            "{} 连接 · {} 分片",
            task.connections,
            format_bytes(task.chunk_size_bytes)
        )
        .into(),
        output_dir: non_empty_or(task.output_dir.clone()).into(),
        output_path: non_empty_or(task.output_path.clone()).into(),
        current_file: task
            .current_file
            .clone()
            .map(non_empty_or)
            .unwrap_or_else(|| "-".to_string())
            .into(),
        error_text: task
            .error
            .clone()
            .map(non_empty_or)
            .unwrap_or_else(|| "-".to_string())
            .into(),
        url: non_empty_or(task.url.clone()).into(),
        created_at: format_time(task.created_at).into(),
        updated_at: format_time(task.updated_at).into(),
        progress_value: progress_value as f32,
        can_pause: can_pause(&task.status),
        can_resume: matches!(task.status, TaskStatus::Paused),
        can_cancel: !is_terminal(&task.status),
        can_open: matches!(task.status, TaskStatus::Completed),
        can_retry: matches!(
            task.status,
            TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Completed
        ),
        can_delete: is_terminal(&task.status),
    }
}

#[derive(Clone)]
struct RequestSettings {
    output_dir: Option<String>,
    password: Option<String>,
    connections: usize,
    chunk_size_mb: u64,
    performance_mode: PerformanceMode,
    retry_mode: RetryMode,
    overwrite: bool,
    verify_integrity: bool,
}

impl RequestSettings {
    fn from_ui(ui: &MegaDownWindow) -> Self {
        let performance_mode = match ui.get_performance_mode_index() {
            1 => PerformanceMode::Fast,
            2 => PerformanceMode::LowImpact,
            _ => PerformanceMode::Balanced,
        };
        let retry_mode = if ui.get_retry_mode_index() == 1 {
            RetryMode::Manual
        } else {
            RetryMode::Auto
        };

        Self {
            output_dir: non_empty(ui.get_output_dir().as_str()),
            password: non_empty(ui.get_password_text().as_str()),
            connections: indexed_value(
                &CONNECTION_OPTIONS,
                ui.get_connections_index(),
                CONNECTION_OPTIONS[2],
            ),
            chunk_size_mb: indexed_value(
                &CHUNK_OPTIONS_MB,
                ui.get_chunk_size_index(),
                CHUNK_OPTIONS_MB[2],
            ),
            performance_mode,
            retry_mode,
            overwrite: ui.get_overwrite_enabled(),
            verify_integrity: ui.get_verify_enabled(),
        }
    }

    fn download_request(&self, url: String) -> DownloadRequest {
        DownloadRequest {
            url,
            output_dir: self.output_dir.clone(),
            password: self.password.clone(),
            connections: Some(self.connections),
            chunk_size_mb: Some(self.chunk_size_mb),
            overwrite: Some(self.overwrite),
            verify_integrity: Some(self.verify_integrity),
            performance_mode: Some(self.performance_mode),
            retry_mode: Some(self.retry_mode),
            low_cpu_mode: Some(matches!(self.performance_mode, PerformanceMode::LowImpact)),
        }
    }
}

fn indexed_value<T: Copy>(values: &[T], index: i32, fallback: T) -> T {
    values
        .get(index.max(0) as usize)
        .copied()
        .unwrap_or(fallback)
}

fn extract_mega_links(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut links = Vec::new();

    for token in text.split(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>')) {
        let cleaned = clean_link(token);
        if cleaned.is_empty() {
            continue;
        }
        if seen.insert(cleaned.clone()) {
            links.push(cleaned);
        }
    }

    links
}

fn clean_link(value: &str) -> String {
    let mut cleaned = value
        .trim()
        .trim_matches(|ch| matches!(ch, ')' | ']' | ',' | ';' | '.'))
        .to_string();
    if cleaned.is_empty() {
        return cleaned;
    }
    let lower = cleaned.to_ascii_lowercase();
    if !lower.contains("mega.nz/") && !lower.contains("mega.co.nz/") {
        return String::new();
    }
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        cleaned = format!("https://{cleaned}");
    }
    cleaned
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_or(value: String) -> String {
    if value.trim().is_empty() {
        "-".to_string()
    } else {
        value
    }
}

fn task_matches_filter(task: &TaskSnapshot, filter_index: i32) -> bool {
    match filter_index {
        1 => is_active(&task.status),
        2 => matches!(task.status, TaskStatus::Paused),
        3 => matches!(task.status, TaskStatus::Completed),
        4 => matches!(task.status, TaskStatus::Failed),
        5 => matches!(task.status, TaskStatus::Cancelled),
        _ => true,
    }
}

fn is_active(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Queued
            | TaskStatus::Resolving
            | TaskStatus::Downloading
            | TaskStatus::Verifying
    )
}

fn can_pause(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Queued
            | TaskStatus::Resolving
            | TaskStatus::Downloading
            | TaskStatus::Verifying
    )
}

fn is_terminal(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn status_text(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "排队中",
        TaskStatus::Resolving => "解析中",
        TaskStatus::Downloading => "下载中",
        TaskStatus::Paused => "已暂停",
        TaskStatus::Verifying => "校验中",
        TaskStatus::Completed => "完成",
        TaskStatus::Failed => "失败",
        TaskStatus::Cancelled => "已取消",
    }
}

fn status_kind(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued
        | TaskStatus::Resolving
        | TaskStatus::Downloading
        | TaskStatus::Verifying => "active",
        TaskStatus::Paused => "paused",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn format_eta(status: &TaskStatus, remaining_bytes: u64, speed_bps: u64) -> String {
    match status {
        TaskStatus::Completed => "已完成".to_string(),
        TaskStatus::Failed => "失败".to_string(),
        TaskStatus::Cancelled => "已取消".to_string(),
        TaskStatus::Paused => "已暂停".to_string(),
        _ if remaining_bytes == 0 || speed_bps == 0 => "计算中".to_string(),
        _ => format!("剩余 {}", format_duration(remaining_bytes / speed_bps)),
    }
}

fn format_duration(seconds: u64) -> String {
    let total_seconds = seconds.max(1);
    if total_seconds < 60 {
        return format!("{total_seconds} 秒");
    }

    let minutes = total_seconds / 60;
    let rest_seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes} 分 {rest_seconds} 秒");
    }

    let hours = minutes / 60;
    let rest_minutes = minutes % 60;
    format!("{hours} 小时 {rest_minutes} 分")
}

fn format_bytes(value: u64) -> String {
    if value < 1024 {
        return format!("{value} B");
    }

    let units = ["KB", "MB", "GB", "TB", "PB"];
    let mut size = value as f64 / 1024.0;
    let mut index = 0usize;
    while size >= 1024.0 && index < units.len() - 1 {
        size /= 1024.0;
        index += 1;
    }

    if size >= 100.0 {
        format!("{:.0} {}", size, units[index])
    } else {
        format!("{:.1} {}", size, units[index])
    }
}

fn format_time(value: u128) -> String {
    if value == 0 {
        return "-".to_string();
    }

    let Ok(duration) = u64::try_from(value) else {
        return "-".to_string();
    };
    let system_time = std::time::UNIX_EPOCH + Duration::from_millis(duration);
    match system_time.duration_since(std::time::UNIX_EPOCH) {
        Ok(_) => {
            let datetime: chrono_like::DateTime = system_time.into();
            datetime.format()
        }
        Err(_) => "-".to_string(),
    }
}

fn chunk_mb_from_bytes(value: u64) -> u64 {
    if value == 0 {
        return CHUNK_OPTIONS_MB[2];
    }
    (value / (1024 * 1024)).max(1)
}

fn set_notice(
    ui: &MegaDownWindow,
    title: impl Into<SharedString>,
    text: impl Into<SharedString>,
    danger: bool,
) {
    ui.set_notice_title(title.into());
    ui.set_notice_text(text.into());
    ui.set_notice_is_error(danger);
}

mod chrono_like {
    use std::time::{SystemTime, UNIX_EPOCH};

    pub struct DateTime {
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    }

    impl DateTime {
        pub fn format(&self) -> String {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                self.year, self.month, self.day, self.hour, self.minute, self.second
            )
        }
    }

    impl From<SystemTime> for DateTime {
        fn from(value: SystemTime) -> Self {
            let seconds = value
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            from_unix_seconds(seconds)
        }
    }

    fn from_unix_seconds(seconds: i64) -> DateTime {
        let days = seconds.div_euclid(86_400);
        let seconds_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        DateTime {
            year,
            month,
            day,
            hour: (seconds_of_day / 3600) as u32,
            minute: ((seconds_of_day % 3600) / 60) as u32,
            second: (seconds_of_day % 60) as u32,
        }
    }

    fn civil_from_days(days: i64) -> (i32, u32, u32) {
        let days = days + 719_468;
        let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
        let doe = days - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        let year = year + if month <= 2 { 1 } else { 0 };
        (year as i32, month as u32, day as u32)
    }
}
