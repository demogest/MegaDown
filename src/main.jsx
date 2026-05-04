import React, { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Activity,
  AlertTriangle,
  Ban,
  CheckCircle2,
  Copy,
  DownloadCloud,
  Eraser,
  FileText,
  FolderOpen,
  Gauge,
  HardDriveDownload,
  Inbox,
  Layers3,
  Leaf,
  Link2,
  ListChecks,
  ListFilter,
  Pause,
  PauseCircle,
  Play,
  PlayCircle,
  RefreshCw,
  RotateCcw,
  Search,
  ShieldCheck,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import "./styles.css";

const SETTINGS_KEY = "megadown.settings.v1";
const ACTIVE_STATUSES = new Set(["queued", "resolving", "downloading", "verifying"]);
const TERMINAL_STATUSES = new Set(["completed", "failed", "cancelled"]);
const PAUSABLE_STATUSES = new Set(["queued", "resolving", "downloading", "verifying"]);
const RETRYABLE_STATUSES = new Set(["failed", "cancelled"]);
const REDOWNLOAD_STATUSES = new Set(["completed"]);

const statusText = {
  queued: "排队中",
  resolving: "解析中",
  downloading: "下载中",
  paused: "已暂停",
  verifying: "校验中",
  completed: "完成",
  failed: "失败",
  cancelled: "已取消",
};

const filterText = {
  all: "全部",
  active: "下载中",
  paused: "已暂停",
  completed: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

const filterIcons = {
  all: ListFilter,
  active: Activity,
  paused: PauseCircle,
  completed: CheckCircle2,
  failed: AlertTriangle,
  cancelled: Ban,
};

const performancePresets = {
  balanced: { connections: "8", chunkSize: "8" },
  fast: { connections: "16", chunkSize: "16" },
  lowImpact: { connections: "4", chunkSize: "4" },
};

const defaultSettings = {
  outputDir: "",
  performanceMode: "balanced",
  connections: "8",
  chunkSize: "8",
  retryMode: "auto",
  overwrite: false,
  verifyIntegrity: false,
};

function tauriInvoke(command, payload) {
  return window.__TAURI__.core.invoke(command, payload);
}

async function writeClipboardText(text) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.left = "-9999px";
  document.body.appendChild(textarea);
  textarea.select();
  document.execCommand("copy");
  textarea.remove();
}

function App() {
  const [tasks, setTasks] = useState([]);
  const [urlText, setUrlText] = useState("");
  const [password, setPassword] = useState("");
  const [settings, setSettings] = useState(defaultSettings);
  const [notice, setNotice] = useState(null);
  const [currentFilter, setCurrentFilter] = useState("all");
  const [selectedTaskId, setSelectedTaskId] = useState(null);
  const [isBusy, setBusy] = useState(false);
  const [globalBusy, setGlobalBusy] = useState(false);
  const [forcePassword, setForcePassword] = useState(false);
  const [activeTab, setActiveTab] = useState("download");
  const [taskMenu, setTaskMenu] = useState(null);

  const pendingLinks = useMemo(() => extractMegaLinks(urlText), [urlText]);
  const counts = useMemo(() => countFilters(tasks), [tasks]);
  const filteredTasks = useMemo(
    () => tasks.filter((task) => taskMatchesFilter(task, currentFilter)),
    [currentFilter, tasks],
  );
  const derivedTasks = useMemo(() => filteredTasks.map(deriveTask), [filteredTasks]);
  const selectedTask = useMemo(
    () => tasks.find((task) => task.id === selectedTaskId) || null,
    [selectedTaskId, tasks],
  );
  const activeTasks = useMemo(
    () => tasks.filter((task) => ACTIVE_STATUSES.has(task.status)),
    [tasks],
  );
  const currentTaskCandidates = useMemo(
    () => tasks.filter((task) => !TERMINAL_STATUSES.has(task.status)),
    [tasks],
  );
  const currentDownloadTasks = useMemo(
    () =>
      currentTaskCandidates
        .slice(-3)
        .reverse()
        .map(deriveTask),
    [currentTaskCandidates],
  );
  const currentPausableIds = useMemo(
    () =>
      currentTaskCandidates
        .filter((task) => PAUSABLE_STATUSES.has(task.status))
        .map((task) => task.id),
    [currentTaskCandidates],
  );
  const currentCancelableIds = useMemo(
    () => currentTaskCandidates.map((task) => task.id),
    [currentTaskCandidates],
  );
  const clearableCount = useMemo(
    () => tasks.filter((task) => TERMINAL_STATUSES.has(task.status)).length,
    [tasks],
  );
  const totalSpeed = useMemo(
    () => activeTasks.reduce((sum, task) => sum + Number(task.speedBps || 0), 0),
    [activeTasks],
  );
  const hasPausable = useMemo(
    () => tasks.some((task) => PAUSABLE_STATUSES.has(task.status)),
    [tasks],
  );
  const hasPaused = useMemo(() => tasks.some((task) => task.status === "paused"), [tasks]);
  const showPassword =
    forcePassword || pendingLinks.some(needsPassword) || password.trim().length > 0;

  const showNotice = useCallback((title, text, danger = false) => {
    setNotice({ title, text, danger });
  }, []);

  const closeTaskMenu = useCallback(() => {
    setTaskMenu(null);
  }, []);

  const saveSettings = useCallback((nextSettings) => {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(nextSettings));
  }, []);

  const updateSettings = useCallback(
    (patch) => {
      setSettings((previous) => {
        const next = { ...previous, ...patch };
        saveSettings(next);
        return next;
      });
    },
    [saveSettings],
  );

  const selectFallbackTask = useCallback((taskList, preferFilter = false, filter = currentFilter) => {
    if (!taskList.length) {
      setSelectedTaskId(null);
      return;
    }

    setSelectedTaskId((currentId) => {
      const selectedExists = taskList.some((task) => task.id === currentId);
      const filtered = taskList.filter((task) => taskMatchesFilter(task, filter));
      const selectedInFilter = filtered.some((task) => task.id === currentId);
      if (selectedExists && (!preferFilter || selectedInFilter)) return currentId;
      const fallback = preferFilter
        ? filtered[filtered.length - 1] || taskList[taskList.length - 1]
        : taskList[taskList.length - 1];
      return fallback ? fallback.id : null;
    });
  }, [currentFilter]);

  const refreshTasks = useCallback(async () => {
    try {
      const nextTasks = await tauriInvoke("list_tasks");
      setTasks(nextTasks);
      selectFallbackTask(nextTasks, true);
    } catch (error) {
      showNotice("队列刷新失败", String(error), true);
    }
  }, [selectFallbackTask, showNotice]);

  useEffect(() => {
    let mounted = true;
    async function boot() {
      let defaultOutputDir = "";
      try {
        defaultOutputDir = await tauriInvoke("default_download_dir");
      } catch (error) {
        showNotice("默认目录读取失败", String(error), true);
      }

      let saved = {};
      try {
        saved = JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}");
      } catch {
        saved = {};
      }

      if (mounted) {
        setSettings({ ...defaultSettings, outputDir: defaultOutputDir, ...saved });
        await refreshTasks();
      }
    }

    boot();
    const timer = setInterval(refreshTasks, 900);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [refreshTasks, showNotice]);

  useEffect(() => {
    if (!showPassword && password) setPassword("");
  }, [password, showPassword]);

  useEffect(() => {
    const close = () => closeTaskMenu();
    const closeOnEscape = (event) => {
      if (event.key === "Escape") closeTaskMenu();
    };

    window.addEventListener("click", close);
    window.addEventListener("resize", close);
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("resize", close);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [closeTaskMenu]);

  useEffect(() => {
    const preventDefaultMenu = (event) => {
      const target = event.target;
      if (!(target instanceof Element)) return;
      if (target.closest("input, textarea, select")) return;
      if (target.closest(".task-row")) return;
      event.preventDefault();
      closeTaskMenu();
    };

    window.addEventListener("contextmenu", preventDefaultMenu);
    return () => window.removeEventListener("contextmenu", preventDefaultMenu);
  }, [closeTaskMenu]);

  const chooseDownloadDir = async () => {
    try {
      const selected = await tauriInvoke("choose_download_dir");
      if (selected) updateSettings({ outputDir: selected });
    } catch (error) {
      showNotice("选择目录失败", String(error), true);
    }
  };

  const applyPerformancePreset = (mode) => {
    const preset = performancePresets[mode] || performancePresets.balanced;
    updateSettings({
      performanceMode: mode,
      connections: preset.connections,
      chunkSize: preset.chunkSize,
    });
  };

  const inspectLink = async () => {
    if (!pendingLinks.length) return showNotice("缺少链接", "请输入 MEGA 公共链接。", true);
    const url = pendingLinks[0];
    if (needsPassword(url) && !readPassword(password)) {
      setForcePassword(true);
      return showNotice("需要密码", "请输入共享密码后再读取。", true);
    }

    setBusy(true);
    try {
      const info = await tauriInvoke("inspect_link", { url, password: readPassword(password) });
      if (info.requiresPassword) {
        setForcePassword(true);
        return showNotice("需要密码", "请输入共享密码后再读取。", true);
      }

      const kind = info.kind === "folder" ? "文件夹" : "文件";
      const suffix =
        pendingLinks.length > 1 ? `；本次仅预览第 1 个，共识别 ${pendingLinks.length} 个链接` : "";
      showNotice(`${kind}: ${info.name}`, `${formatBytes(info.totalBytes)} · ${info.fileCount} 个文件${suffix}`);
    } catch (error) {
      showNotice("链接解析失败", String(error), true);
    } finally {
      setBusy(false);
    }
  };

  const startDownload = async (linksOverride = null, settingsOverride = null) => {
    const links = Array.isArray(linksOverride) ? linksOverride : pendingLinks;
    const effectiveSettings = settingsOverride
      ? { ...settings, ...settingsOverride }
      : settings;
    if (!links.length) return showNotice("缺少链接", "请输入 MEGA 公共链接。", true);
    if (links.some(needsPassword) && !readPassword(password)) {
      setForcePassword(true);
      return showNotice("需要密码", "请输入共享密码后再下载。", true);
    }

    setBusy(true);
    const failed = [];
    let added = 0;

    for (const url of links) {
      try {
        const task = await tauriInvoke("start_download", {
          request: {
            url,
            outputDir: String(effectiveSettings.outputDir || "").trim(),
            password: readPassword(password),
            connections: Number(effectiveSettings.connections),
            chunkSizeMb: Number(effectiveSettings.chunkSize),
            performanceMode: effectiveSettings.performanceMode,
            retryMode: effectiveSettings.retryMode,
            overwrite: Boolean(effectiveSettings.overwrite),
            verifyIntegrity: Boolean(effectiveSettings.verifyIntegrity),
            lowCpuMode: effectiveSettings.performanceMode === "lowImpact",
          },
        });
        added += 1;
        if (task?.id) setSelectedTaskId(task.id);
      } catch (error) {
        failed.push({ url, error: String(error) });
      }
    }

    if (failed.length) {
      setUrlText(failed.map((item) => item.url).join("\n"));
      const title = added ? `已添加 ${added} 个，失败 ${failed.length} 个` : "无法开始下载";
      showNotice(title, `首个错误：${failed[0].error}`, !added);
    } else {
      setUrlText("");
      setPassword("");
      setForcePassword(false);
      showNotice("已加入队列", `${added} 个链接已开始下载。`);
    }

    await refreshTasks();
    setBusy(false);
  };

  const invokeTaskAction = async (command, id, title) => {
    if (!id) return;
    try {
      await tauriInvoke(command, { id });
    } catch (error) {
      showNotice(`${title}失败`, String(error), true);
    } finally {
      await refreshTasks();
    }
  };

  const restartTask = async (task, { redownload = false } = {}) => {
    if (!task?.url) {
      showNotice(redownload ? "无法重新下载" : "无法重试", "任务缺少原始链接。", true);
      return;
    }

    setSelectedTaskId(task.id);
    setUrlText(task.url);
    if (needsPassword(task.url) && !readPassword(password)) {
      setActiveTab("download");
      setForcePassword(true);
      showNotice("需要密码", "请输入共享密码后再重试。", true);
      return;
    }

    await startDownload([task.url], taskSettingsFromSnapshot(task, settings, { redownload }));
  };

  const retryTask = (task) => restartTask(task);
  const redownloadTask = (task) => restartTask(task, { redownload: true });

  const deleteTask = async (id) => {
    if (!id) return;
    try {
      await tauriInvoke("delete_task", { id });
      showNotice("任务已删除", "任务记录已从列表移除。");
    } catch (error) {
      showNotice("删除任务失败", String(error), true);
    } finally {
      await refreshTasks();
    }
  };

  const clearFinishedTasks = async () => {
    setGlobalBusy(true);
    try {
      const removed = await tauriInvoke("clear_finished_tasks");
      showNotice(
        removed ? "已清空任务" : "没有可清空任务",
        removed ? `已移除 ${removed} 个已结束任务记录。` : "运行中任务会保留在队列中。",
      );
    } catch (error) {
      showNotice("清空任务失败", String(error), true);
    } finally {
      await refreshTasks();
      setGlobalBusy(false);
    }
  };

  const openTaskTarget = async (command, task, title) => {
    if (!task?.id) return;
    try {
      await tauriInvoke(command, { id: task.id });
      showNotice(title, task.fileName || "任务路径已打开。");
    } catch (error) {
      showNotice(`${title}失败`, String(error), true);
    }
  };

  const invokeGlobalAction = async (command, title) => {
    setGlobalBusy(true);
    try {
      await tauriInvoke(command);
    } catch (error) {
      showNotice(`${title}失败`, String(error), true);
    } finally {
      await refreshTasks();
      setGlobalBusy(false);
    }
  };

  const invokeTaskBatchAction = async (command, ids, title) => {
    const uniqueIds = Array.from(new Set(ids)).filter(Boolean);
    if (!uniqueIds.length) return;

    setGlobalBusy(true);
    const failed = [];
    try {
      for (const id of uniqueIds) {
        try {
          await tauriInvoke(command, { id });
        } catch (error) {
          failed.push(String(error));
        }
      }

      if (failed.length) {
        showNotice(`${title}部分失败`, `失败 ${failed.length} 个；首个错误：${failed[0]}`, true);
      } else {
        showNotice(`${title}已执行`, `${uniqueIds.length} 个任务已处理。`);
      }
    } finally {
      await refreshTasks();
      setGlobalBusy(false);
    }
  };

  const openTaskMenu = useCallback((event, task) => {
    event.preventDefault();
    event.stopPropagation();
    setSelectedTaskId(task.id);

    const menuWidth = 226;
    const menuHeight = 324;
    setTaskMenu({
      x: Math.max(8, Math.min(event.clientX, window.innerWidth - menuWidth - 8)),
      y: Math.max(8, Math.min(event.clientY, window.innerHeight - menuHeight - 8)),
      task,
    });
  }, []);

  const copyTaskValue = useCallback(
    async (label, value) => {
      closeTaskMenu();
      if (!value) {
        showNotice("没有可复制内容", label, true);
        return;
      }

      try {
        await writeClipboardText(value);
        showNotice("已复制", label);
      } catch (error) {
        showNotice("复制失败", String(error), true);
      }
    },
    [closeTaskMenu, showNotice],
  );

  const changeFilter = (filter) => {
    setCurrentFilter(filter);
    selectFallbackTask(tasks, true, filter);
  };

  const queueSummary = tasks.length
    ? `${filterText[currentFilter] || "全部"} · ${filteredTasks.length} / ${tasks.length} 个任务`
    : "暂无任务";
  const contextTask = taskMenu
    ? deriveTask(tasks.find((task) => task.id === taskMenu.task.id) || taskMenu.task)
    : null;

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark">
            <HardDriveDownload size={22} strokeWidth={2.4} />
          </div>
          <div className="brand-copy">
            <h1>MegaDown</h1>
            <p>MEGA 下载管理器</p>
          </div>
        </div>

        <div className="topbar-right">
          <TabNav activeTab={activeTab} counts={counts} onChange={setActiveTab} />
        </div>
      </header>

      {notice && (
        <section className={`notice ${notice.danger ? "danger" : ""}`}>
          <strong>{notice.title}</strong>
          <span>{notice.text}</span>
        </section>
      )}

      <section className="tab-content">
        {activeTab === "download" && (
          <DownloadPage
            isBusy={isBusy}
            outputDir={settings.outputDir}
            password={password}
            pendingLinks={pendingLinks}
            showPassword={showPassword}
            urlText={urlText}
            currentTasks={currentDownloadTasks}
            globalBusy={globalBusy}
            hasCurrentPausable={currentPausableIds.length > 0}
            hasCurrentCancelable={currentCancelableIds.length > 0}
            onChooseDir={chooseDownloadDir}
            onCancelAllCurrent={() =>
              invokeTaskBatchAction("cancel_download", currentCancelableIds, "全部取消")
            }
            onCancelTask={(id) => invokeTaskAction("cancel_download", id, "取消")}
            onInspect={inspectLink}
            onOpenTasks={() => setActiveTab("tasks")}
            onPauseAllCurrent={() =>
              invokeTaskBatchAction("pause_download", currentPausableIds, "全部暂停")
            }
            onPauseTask={(id) => invokeTaskAction("pause_download", id, "暂停")}
            onPasswordChange={setPassword}
            onResumeTask={(id) => invokeTaskAction("resume_download", id, "继续")}
            onStart={startDownload}
            onUrlChange={(value) => {
              setUrlText(value);
              setForcePassword(false);
            }}
            onOutputDirChange={(value) => updateSettings({ outputDir: value })}
          />
        )}

        {activeTab === "tasks" && (
          <TasksPage
            activeTasks={activeTasks}
            counts={counts}
            clearableCount={clearableCount}
            currentFilter={currentFilter}
            derivedTasks={derivedTasks}
            globalBusy={globalBusy}
            hasPausable={hasPausable}
            hasPaused={hasPaused}
            isBusy={isBusy}
            queueSummary={queueSummary}
            selectedTask={selectedTask ? deriveTask(selectedTask) : null}
            selectedTaskId={selectedTaskId}
            tasksTotal={tasks.length}
            totalSpeed={totalSpeed}
            onCancel={(id) => invokeTaskAction("cancel_download", id, "取消")}
            onChangeFilter={changeFilter}
            onClearFinished={clearFinishedTasks}
            onDelete={deleteTask}
            onOpenFile={(task) => openTaskTarget("open_task_file", task, "打开文件")}
            onOpenFolder={(task) => openTaskTarget("open_task_folder", task, "打开文件夹")}
            onPause={(id) => invokeTaskAction("pause_download", id, "暂停")}
            onPauseAll={() => invokeGlobalAction("pause_all_downloads", "全部暂停")}
            onRedownload={redownloadTask}
            onRefresh={refreshTasks}
            onRetry={retryTask}
            onResume={(id) => invokeTaskAction("resume_download", id, "继续")}
            onResumeAll={() => invokeGlobalAction("resume_all_downloads", "全部继续")}
            onSelect={setSelectedTaskId}
            onTaskContextMenu={openTaskMenu}
          />
        )}

        {activeTab === "settings" && (
          <SettingsPage
            settings={settings}
            onModeChange={applyPerformancePreset}
            onUpdateSettings={updateSettings}
          />
        )}
      </section>

      {contextTask && (
        <TaskContextMenu
          task={contextTask}
          x={taskMenu.x}
          y={taskMenu.y}
          onCancel={(id) => {
            closeTaskMenu();
            invokeTaskAction("cancel_download", id, "取消");
          }}
          onClose={closeTaskMenu}
          onCopyLink={(task) => copyTaskValue("任务链接", task.url)}
          onCopyPath={(task) => copyTaskValue("保存路径", task.outputPath)}
          onDelete={(id) => {
            closeTaskMenu();
            deleteTask(id);
          }}
          onOpenFile={(task) => {
            closeTaskMenu();
            openTaskTarget("open_task_file", task, "打开文件");
          }}
          onOpenFolder={(task) => {
            closeTaskMenu();
            openTaskTarget("open_task_folder", task, "打开文件夹");
          }}
          onPause={(id) => {
            closeTaskMenu();
            invokeTaskAction("pause_download", id, "暂停");
          }}
          onRedownload={(task) => {
            closeTaskMenu();
            redownloadTask(task);
          }}
          onRetry={(task) => {
            closeTaskMenu();
            retryTask(task);
          }}
          onResume={(id) => {
            closeTaskMenu();
            invokeTaskAction("resume_download", id, "继续");
          }}
        />
      )}
    </main>
  );
}

function TabNav({ activeTab, counts, onChange }) {
  const tabs = [
    { id: "download", label: "下载", icon: DownloadCloud },
    { id: "tasks", label: "任务", icon: ListChecks, badge: counts.all },
    { id: "settings", label: "参数", icon: Gauge },
  ];

  return (
    <nav className="tab-nav" aria-label="主界面">
      {tabs.map((tab) => {
        const Icon = tab.icon;
        return (
          <button
            key={tab.id}
            className={`tab-button ${activeTab === tab.id ? "active" : ""}`}
            onClick={() => onChange(tab.id)}
            type="button"
          >
            <Icon size={15} />
            <span>{tab.label}</span>
            {typeof tab.badge === "number" && <strong>{tab.badge}</strong>}
          </button>
        );
      })}
    </nav>
  );
}

function DownloadPage({
  currentTasks,
  globalBusy,
  hasCurrentCancelable,
  hasCurrentPausable,
  isBusy,
  outputDir,
  password,
  pendingLinks,
  showPassword,
  urlText,
  onChooseDir,
  onCancelAllCurrent,
  onCancelTask,
  onInspect,
  onOpenTasks,
  onOutputDirChange,
  onPauseAllCurrent,
  onPauseTask,
  onPasswordChange,
  onResumeTask,
  onStart,
  onUrlChange,
}) {
  return (
    <section className={`tab-page download-page ${currentTasks.length ? "has-current" : "is-idle"}`}>
      <div className="download-card">
        <div className="download-entry">
          <div className="input-stack">
            <label htmlFor="url">MEGA 链接</label>
            <textarea
              id="url"
              spellCheck="false"
              rows="5"
              placeholder="粘贴一个或多个 MEGA 链接，每行一个"
              value={urlText}
              onChange={(event) => onUrlChange(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
                  event.preventDefault();
                  onStart();
                }
              }}
            />
          </div>
          <div className="link-meta">
            <span>{pendingLinks.length ? `${pendingLinks.length} 个链接待添加` : "0 个链接待添加"}</span>
          </div>
        </div>

        <div className="download-actions">
          <button className="secondary command-button" disabled={isBusy} onClick={onInspect}>
            <Search size={16} />
            读取信息
          </button>
          <button className="primary command-button" disabled={isBusy} onClick={onStart}>
            <DownloadCloud size={17} />
            {isBusy ? "处理中" : "开始下载"}
          </button>
        </div>

        <div className={`download-fields ${showPassword ? "has-password" : "no-password"}`}>
          <div className="input-stack path-field">
            <label htmlFor="output-dir">保存到</label>
            <div className="path-picker">
              <input
                id="output-dir"
                type="text"
                spellCheck="false"
                value={outputDir}
                onChange={(event) => onOutputDirChange(event.target.value)}
              />
              <button className="icon-button" title="选择文件夹" onClick={onChooseDir}>
                <FolderOpen size={16} />
              </button>
            </div>
          </div>

          <div className={`input-stack password-field ${showPassword ? "" : "hidden"}`}>
            <label htmlFor="link-password">共享密码</label>
            <input
              id="link-password"
              type="password"
              autoComplete="off"
              spellCheck="false"
              value={password}
              onChange={(event) => onPasswordChange(event.target.value)}
            />
          </div>
        </div>
      </div>

      {currentTasks.length > 0 && (
        <CurrentTasksPanel
          globalBusy={globalBusy}
          hasCancelable={hasCurrentCancelable}
          hasPausable={hasCurrentPausable}
          tasks={currentTasks}
          onCancelAll={onCancelAllCurrent}
          onCancelTask={onCancelTask}
          onOpenTasks={onOpenTasks}
          onPauseAll={onPauseAllCurrent}
          onPauseTask={onPauseTask}
          onResumeTask={onResumeTask}
        />
      )}
    </section>
  );
}

function CurrentTasksPanel({
  globalBusy,
  hasCancelable,
  hasPausable,
  tasks,
  onCancelAll,
  onCancelTask,
  onOpenTasks,
  onPauseAll,
  onPauseTask,
  onResumeTask,
}) {
  return (
    <section className="current-panel">
      <div className="current-head">
        <div>
          <h2>当前任务</h2>
          <p>{tasks.length} 个任务正在处理</p>
        </div>
        <div className="current-head-actions">
          <button className="secondary command-button" disabled={!hasPausable || globalBusy} onClick={onPauseAll}>
            <Pause size={15} />
            全部暂停
          </button>
          <button className="secondary command-button danger-lite" disabled={!hasCancelable || globalBusy} onClick={onCancelAll}>
            <X size={15} />
            全部取消
          </button>
          <button className="secondary command-button" onClick={onOpenTasks}>
            <ListChecks size={15} />
            查看全部
          </button>
        </div>
      </div>

      <div className="current-list">
        {tasks.map((task) => (
          <article className="current-task" key={task.id}>
            <div className="current-task-main">
              <span className={`status-dot ${task.status}`} />
              <div className="task-name">
                <strong title={task.fileName}>
                  <Link2 size={14} />
                  {task.fileName}
                </strong>
                <span title={task.currentFile || task.outputPath || task.url}>
                  {task.currentFile || task.outputPath || task.url}
                </span>
              </div>
            </div>

            <div className="current-task-progress">
              <ProgressBar task={task} />
              <div className="progress-meta">
                <span>{statusText[task.status] || task.status} · {task.percent.toFixed(1)}%</span>
                <span>{formatBytes(task.speedBps)}/s · {task.etaText}</span>
              </div>
            </div>

            <div className="current-task-actions">
              {task.status === "paused" ? (
                <button className="current-action resume" onClick={() => onResumeTask(task.id)} title="继续">
                  <Play size={14} />
                  继续
                </button>
              ) : (
                <button
                  className="current-action"
                  disabled={!PAUSABLE_STATUSES.has(task.status)}
                  onClick={() => onPauseTask(task.id)}
                  title="暂停"
                >
                  <Pause size={14} />
                  暂停
                </button>
              )}
              <button className="current-action cancel" onClick={() => onCancelTask(task.id)} title="取消">
                <X size={14} />
                取消
              </button>
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}

function TasksPage({
  activeTasks,
  counts,
  clearableCount,
  currentFilter,
  derivedTasks,
  globalBusy,
  hasPausable,
  hasPaused,
  isBusy,
  queueSummary,
  selectedTask,
  selectedTaskId,
  tasksTotal,
  totalSpeed,
  onCancel,
  onChangeFilter,
  onClearFinished,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onPause,
  onPauseAll,
  onRedownload,
  onRefresh,
  onRetry,
  onResume,
  onResumeAll,
  onSelect,
  onTaskContextMenu,
}) {
  return (
    <section className="tab-page tasks-page">
      <div className="task-toolbar">
        <div className="task-toolbar-actions">
          <button className="secondary command-button" disabled={!hasPausable || isBusy || globalBusy} onClick={onPauseAll}>
            <PauseCircle size={16} />
            全部暂停
          </button>
          <button className="secondary command-button" disabled={!hasPaused || isBusy || globalBusy} onClick={onResumeAll}>
            <PlayCircle size={16} />
            全部继续
          </button>
          <button className="secondary command-button" disabled={!clearableCount || isBusy || globalBusy} onClick={onClearFinished}>
            <Eraser size={16} />
            清空已结束
          </button>
          <button className="icon-button" title="刷新任务" onClick={onRefresh}>
            <RefreshCw size={16} />
          </button>
        </div>

        <div className="stats">
          <StatCard icon={Activity} value={activeTasks.length} label="活动" />
          <StatCard icon={Gauge} value={`${formatBytes(totalSpeed)}/s`} label="总速度" />
          <StatCard icon={CheckCircle2} value={counts.completed} label="完成" />
          <StatCard icon={AlertTriangle} value={counts.failed} label="失败" />
        </div>
      </div>

      <section className="workspace">
        <FilterPanel currentFilter={currentFilter} counts={counts} onChange={onChangeFilter} />
        <TaskList
          tasks={derivedTasks}
          total={tasksTotal}
          summary={queueSummary}
          selectedTaskId={selectedTaskId}
          onSelect={onSelect}
          onPause={onPause}
          onDelete={onDelete}
          onOpenFile={onOpenFile}
          onOpenFolder={onOpenFolder}
          onRedownload={onRedownload}
          onRetry={onRetry}
          onResume={onResume}
          onCancel={onCancel}
          onTaskContextMenu={onTaskContextMenu}
        />
        <DetailsPanel
          task={selectedTask}
          onPause={onPause}
          onDelete={onDelete}
          onOpenFile={onOpenFile}
          onOpenFolder={onOpenFolder}
          onRedownload={onRedownload}
          onRetry={onRetry}
          onResume={onResume}
          onCancel={onCancel}
        />
      </section>
    </section>
  );
}

function TaskContextMenu({
  task,
  x,
  y,
  onCancel,
  onClose,
  onCopyLink,
  onCopyPath,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onPause,
  onRedownload,
  onRetry,
  onResume,
}) {
  const canPause = PAUSABLE_STATUSES.has(task.status);
  const canResume = task.status === "paused";
  const canCancel = !TERMINAL_STATUSES.has(task.status);
  const canOpen = task.status === "completed";
  const canDelete = TERMINAL_STATUSES.has(task.status);

  const run = (callback) => (event) => {
    event.stopPropagation();
    callback();
  };

  return (
    <div className="context-layer" onContextMenu={(event) => event.preventDefault()}>
      <div
        className="task-context-menu"
        role="menu"
        style={{ left: x, top: y }}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="task-menu-head">
          <strong title={task.fileName}>{task.fileName}</strong>
          <span>{statusText[task.status] || task.status}</span>
        </div>

        <div className="task-menu-group">
          {canResume && (
            <button type="button" onClick={run(() => onResume(task.id))}>
              <Play size={15} />
              继续任务
            </button>
          )}
          {canPause && (
            <button type="button" onClick={run(() => onPause(task.id))}>
              <Pause size={15} />
              暂停任务
            </button>
          )}
          {canCancel && (
            <button className="danger" type="button" onClick={run(() => onCancel(task.id))}>
              <X size={15} />
              取消并清理
            </button>
          )}
          {canOpen && (
            <>
              <button type="button" onClick={run(() => onOpenFile(task))}>
                <FileText size={15} />
                打开文件
              </button>
              <button type="button" onClick={run(() => onOpenFolder(task))}>
                <FolderOpen size={15} />
                打开文件夹
              </button>
              <button type="button" onClick={run(() => onRedownload(task))}>
                <RotateCcw size={15} />
                重新下载
              </button>
            </>
          )}
          {RETRYABLE_STATUSES.has(task.status) && (
            <button type="button" onClick={run(() => onRetry(task))}>
              <RefreshCw size={15} />
              重试任务
            </button>
          )}
          {canDelete && (
            <button className="danger" type="button" onClick={run(() => onDelete(task.id))}>
              <Trash2 size={15} />
              删除记录
            </button>
          )}
        </div>

        <div className="task-menu-group">
          <button type="button" onClick={run(() => onCopyLink(task))}>
            <Link2 size={15} />
            复制链接
          </button>
          <button type="button" onClick={run(() => onCopyPath(task))}>
            <Copy size={15} />
            复制保存路径
          </button>
        </div>

        <div className="task-menu-group">
          <button type="button" onClick={run(onClose)}>
            <Ban size={15} />
            关闭菜单
          </button>
        </div>
      </div>
    </div>
  );
}

function SettingsPage({ settings, onModeChange, onUpdateSettings }) {
  return (
    <section className="tab-page settings-page">
      <section className="settings-panel">
        <div className="settings-head">
          <h2>下载参数</h2>
        </div>

        <div className="settings-sections">
          <section className="settings-section performance-section">
            <div className="settings-section-head">
              <h3>性能与传输</h3>
            </div>
            <PerformanceModeControl value={settings.performanceMode} onChange={onModeChange} />
            <div className="settings-two-col">
              <SelectField id="connections" label="连接数" value={settings.connections} className="connections-field" onChange={(value) => onUpdateSettings({ connections: value })}>
                {["2", "4", "8", "12", "16", "24", "32"].map((value) => (
                  <option key={value} value={value}>{value}</option>
                ))}
              </SelectField>

              <SelectField id="chunk-size" label="分片大小" value={settings.chunkSize} className="chunk-field" onChange={(value) => onUpdateSettings({ chunkSize: value })}>
                {["2", "4", "8", "16", "32"].map((value) => (
                  <option key={value} value={value}>{value} MB</option>
                ))}
              </SelectField>
            </div>
          </section>

          <section className="settings-section retry-section">
            <div className="settings-section-head">
              <h3>失败处理</h3>
            </div>
            <RetryModeControl
              value={settings.retryMode}
              onChange={(value) => onUpdateSettings({ retryMode: value })}
            />
          </section>

          <section className="settings-section file-section">
            <div className="settings-section-head">
              <h3>文件处理</h3>
            </div>
            <div className="settings-checks">
              <CheckField icon={Layers3} label="覆盖已有文件" className="overwrite-field" checked={settings.overwrite} onChange={(value) => onUpdateSettings({ overwrite: value })} />
              <CheckField icon={ShieldCheck} label="完整校验" className="integrity-field" checked={settings.verifyIntegrity} onChange={(value) => onUpdateSettings({ verifyIntegrity: value })} />
            </div>
          </section>
        </div>
      </section>
    </section>
  );
}

function StatCard({ icon: Icon, value, label }) {
  return (
    <div className="stat-card">
      <Icon size={15} />
      <div>
        <span>{value}</span>
        <label>{label}</label>
      </div>
    </div>
  );
}

function SelectField({ id, label, value, onChange, children, className = "" }) {
  return (
    <div className={`input-stack short ${className}`}>
      <label htmlFor={id}>{label}</label>
      <select id={id} value={value} onChange={(event) => onChange(event.target.value)}>
        {children}
      </select>
    </div>
  );
}

function PerformanceModeControl({ value, onChange }) {
  const options = [
    { value: "balanced", label: "均衡", icon: Gauge },
    { value: "fast", label: "极速", icon: Zap },
    { value: "lowImpact", label: "低占用", icon: Leaf },
  ];

  return (
    <div className="input-stack mode-field">
      <div className="mode-control">
        {options.map((option) => {
          const Icon = option.icon;
          return (
            <button
              key={option.value}
              className={`mode-option ${value === option.value ? "active" : ""}`}
              onClick={() => onChange(option.value)}
              type="button"
            >
              <Icon size={15} />
              <span>{option.label}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}

function RetryModeControl({ value, onChange }) {
  const options = [
    { value: "auto", label: "自动重试", icon: RefreshCw },
    { value: "manual", label: "手动重试", icon: PlayCircle },
  ];

  return (
    <div className="input-stack retry-field">
      <div className="mode-control retry-control">
        {options.map((option) => {
          const Icon = option.icon;
          return (
            <button
              key={option.value}
              className={`mode-option ${value === option.value ? "active" : ""}`}
              onClick={() => onChange(option.value)}
              type="button"
            >
              <Icon size={15} />
              <span>{option.label}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}

function CheckField({ icon: Icon, label, checked, onChange, className = "" }) {
  return (
    <label className={`check ${className}`}>
      <input type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} />
      <Icon size={15} />
      <span>{label}</span>
    </label>
  );
}

function FilterPanel({ currentFilter, counts, onChange }) {
  return (
    <aside className="filter-panel">
      <div className="panel-title">任务视图</div>
      {["all", "active", "paused", "completed", "failed", "cancelled"].map((filter) => (
        <FilterButton
          key={filter}
          filter={filter}
          active={currentFilter === filter}
          count={counts[filter]}
          onClick={() => onChange(filter)}
        />
      ))}
    </aside>
  );
}

function FilterButton({ filter, active, count, onClick }) {
  const Icon = filterIcons[filter] || ListChecks;
  return (
    <button className={`filter-button ${active ? "active" : ""}`} onClick={onClick}>
      <span>
        <Icon size={15} />
        {filterText[filter]}
      </span>
      <strong>{count}</strong>
    </button>
  );
}

function TaskList({
  tasks,
  total,
  summary,
  selectedTaskId,
  onSelect,
  onPause,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onRedownload,
  onRetry,
  onResume,
  onCancel,
  onTaskContextMenu,
}) {
  return (
    <section className="queue-panel">
      <div className="queue-head">
        <div>
          <h2>下载队列</h2>
          <p>{summary}</p>
        </div>
      </div>
      <div className={`tasks ${tasks.length ? "" : "empty"}`}>
        {tasks.length ? (
          tasks.map((task) => (
            <TaskRow
              key={task.id}
              task={task}
              selected={task.id === selectedTaskId}
              onSelect={onSelect}
              onPause={onPause}
              onDelete={onDelete}
              onOpenFile={onOpenFile}
              onOpenFolder={onOpenFolder}
              onRedownload={onRedownload}
              onRetry={onRetry}
              onResume={onResume}
              onCancel={onCancel}
              onContextMenu={onTaskContextMenu}
            />
          ))
        ) : (
          <EmptyState
            title={total ? "当前视图没有任务" : "暂无任务"}
            text={total ? "切换左侧筛选可查看其他状态。" : "添加公共文件或文件夹链接后会显示进度。"}
          />
        )}
      </div>
    </section>
  );
}

function TaskRow({
  task,
  selected,
  onSelect,
  onPause,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onRedownload,
  onRetry,
  onResume,
  onCancel,
  onContextMenu,
}) {
  const detail = task.error || task.currentFile || task.outputPath || task.url;
  return (
    <article
      className={`task-row ${selected ? "selected" : ""}`}
      onClick={() => onSelect(task.id)}
      onContextMenu={(event) => onContextMenu(event, task)}
    >
      <div className="task-main">
        <span className={`status-dot ${task.status}`} />
        <div className="task-name">
          <strong title={task.fileName}>
            <Link2 size={14} />
            {task.fileName}
          </strong>
          <span title={detail}>{detail}</span>
        </div>
      </div>

      <div className="task-progress">
        <ProgressBar task={task} />
        <div className="progress-meta">
          <span>{statusText[task.status] || task.status} · {task.percent.toFixed(1)}%</span>
          <span>{task.etaText}</span>
        </div>
      </div>

      <div className="task-metrics">
        <span>{formatBytes(task.downloadedBytes)} / {formatBytes(task.totalBytes)}</span>
        <strong>{formatBytes(task.speedBps)}/s</strong>
      </div>

      <div className="task-config">
        <span>{task.connections} 连接</span>
        <span>{formatBytes(task.chunkSizeBytes)} 分片</span>
      </div>

      <TaskActions
        task={task}
        onPause={onPause}
        onDelete={onDelete}
        onOpenFile={onOpenFile}
        onOpenFolder={onOpenFolder}
        onRedownload={onRedownload}
        onRetry={onRetry}
        onResume={onResume}
        onCancel={onCancel}
      />
    </article>
  );
}

function DetailsPanel({
  task,
  onPause,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onRedownload,
  onRetry,
  onResume,
  onCancel,
}) {
  if (!task) {
    return (
      <aside className="details-panel">
        <EmptyState title="未选择任务" text="选择队列中的任务可查看链接、路径和下载参数。" details />
      </aside>
    );
  }

  return (
    <aside className="details-panel">
      <div className="details-head">
        <div>
          <span className={`status-pill ${task.status}`}>{statusText[task.status] || task.status}</span>
          <h2 title={task.fileName}>{task.fileName}</h2>
        </div>
        <div className="detail-actions">
          <TaskActions
            task={task}
            onPause={onPause}
            onDelete={onDelete}
            onOpenFile={onOpenFile}
            onOpenFolder={onOpenFolder}
            onRedownload={onRedownload}
            onRetry={onRetry}
            onResume={onResume}
            onCancel={onCancel}
          />
        </div>
      </div>

      <div className="detail-progress">
        <ProgressBar task={task} />
        <div>
          <strong>{task.percent.toFixed(1)}%</strong>
          <span>{formatBytes(task.speedBps)}/s · {task.etaText}</span>
        </div>
      </div>

      {TERMINAL_STATUSES.has(task.status) && (
        <div className="detail-command-grid">
          {REDOWNLOAD_STATUSES.has(task.status) && (
            <button type="button" className="secondary command-button" onClick={() => onRedownload(task)}>
              <RotateCcw size={15} />
              重新下载
            </button>
          )}
          {RETRYABLE_STATUSES.has(task.status) && (
            <button type="button" className="secondary command-button" onClick={() => onRetry(task)}>
              <RefreshCw size={15} />
              重试任务
            </button>
          )}
          <button type="button" className="secondary command-button danger-lite" onClick={() => onDelete(task.id)}>
            <Trash2 size={15} />
            删除记录
          </button>
        </div>
      )}

      <dl className="detail-grid">
        <DetailRow label="大小" value={`${formatBytes(task.downloadedBytes)} / ${formatBytes(task.totalBytes)}`} />
        <DetailRow label="保存目录" value={task.outputDir || "-"} />
        <DetailRow label="保存路径" value={task.outputPath || "-"} />
        <DetailRow label="当前文件" value={task.currentFile || "-"} />
        <DetailRow label="连接" value={String(task.connections)} />
        <DetailRow label="分片" value={formatBytes(task.chunkSizeBytes)} />
        <DetailRow label="创建时间" value={formatTime(task.createdAt)} />
        <DetailRow label="更新时间" value={formatTime(task.updatedAt)} />
        <DetailRow label="错误" value={task.error || "-"} modifier={task.error ? "danger" : ""} />
        <DetailRow label="链接" value={task.url || "-"} modifier="mono" />
      </dl>
    </aside>
  );
}

function TaskActions({
  task,
  onPause,
  onDelete,
  onOpenFile,
  onOpenFolder,
  onRetry,
  onResume,
  onCancel,
}) {
  const stopWithId = (event, callback) => {
    event.stopPropagation();
    callback(task.id);
  };
  const stopWithTask = (event, callback) => {
    event.stopPropagation();
    callback(task);
  };

  if (task.status === "paused") {
    return (
      <div className="task-actions">
        <button className="small-action resume" onClick={(event) => stopWithId(event, onResume)} title="继续">
          <Play size={14} />
          继续
        </button>
        <button className="small-action cancel" onClick={(event) => stopWithId(event, onCancel)} title="取消">
          <X size={14} />
          取消
        </button>
      </div>
    );
  }

  if (PAUSABLE_STATUSES.has(task.status)) {
    return (
      <div className="task-actions">
        <button className="small-action" onClick={(event) => stopWithId(event, onPause)} title="暂停">
          <Pause size={14} />
          暂停
        </button>
        <button className="small-action cancel" onClick={(event) => stopWithId(event, onCancel)} title="取消">
          <X size={14} />
          取消
        </button>
      </div>
    );
  }

  if (TERMINAL_STATUSES.has(task.status)) {
    if (task.status === "completed") {
      return (
        <div className="task-actions">
          <button className="small-action open" onClick={(event) => stopWithTask(event, onOpenFile)} title="打开文件">
            <FileText size={14} />
            打开
          </button>
          <button className="small-action folder" onClick={(event) => stopWithTask(event, onOpenFolder)} title="打开文件夹">
            <FolderOpen size={14} />
            目录
          </button>
        </div>
      );
    }

    if (RETRYABLE_STATUSES.has(task.status)) {
      return (
        <div className="task-actions">
          <button
            className="small-action resume"
            onClick={(event) => {
              event.stopPropagation();
              onRetry(task);
            }}
            title="重试"
          >
            <RefreshCw size={14} />
            重试
          </button>
          <button className="small-action delete" onClick={(event) => stopWithId(event, onDelete)} title="删除记录">
            <Trash2 size={14} />
            删除
          </button>
        </div>
      );
    }
    return <span className="task-actions-empty">无操作</span>;
  }

  return <span className="task-actions-empty">-</span>;
}

function DetailRow({ label, value, modifier = "" }) {
  return (
    <div className={`detail-row ${modifier}`}>
      <dt>{label}</dt>
      <dd title={value}>{value}</dd>
    </div>
  );
}

function ProgressBar({ task }) {
  return (
    <div className="progress-line">
      <div className={`bar ${task.status}`} style={{ width: `${task.percent.toFixed(2)}%` }} />
    </div>
  );
}

function EmptyState({ title, text }) {
  return (
    <div className="empty-state details-empty">
      <Inbox size={28} />
      <strong>{title}</strong>
      <span>{text}</span>
    </div>
  );
}

function deriveTask(task) {
  const totalBytes = Number(task.totalBytes || 0);
  const downloadedBytes = Number(task.downloadedBytes || 0);
  const speedBps = Number(task.speedBps || 0);
  const remainingBytes = Math.max(totalBytes - downloadedBytes, 0);
  const percent = totalBytes
    ? Math.min(100, (downloadedBytes / totalBytes) * 100)
    : task.status === "completed"
      ? 100
      : 0;

  return {
    ...task,
    totalBytes,
    downloadedBytes,
    speedBps,
    chunkSizeBytes: Number(task.chunkSizeBytes || 0),
    percent,
    etaText: formatEta(task.status, remainingBytes, speedBps),
  };
}

function taskSettingsFromSnapshot(task, fallbackSettings, { redownload = false } = {}) {
  return {
    outputDir: task.outputDir || inferOutputDir(task.outputPath) || fallbackSettings.outputDir,
    performanceMode: task.performanceMode || fallbackSettings.performanceMode,
    connections: String(task.connections || fallbackSettings.connections),
    chunkSize: String(chunkMbFromBytes(task.chunkSizeBytes) || fallbackSettings.chunkSize),
    retryMode: task.retryMode || fallbackSettings.retryMode,
    overwrite: redownload ? true : Boolean(task.overwrite ?? fallbackSettings.overwrite),
    verifyIntegrity: Boolean(task.verifyIntegrity ?? fallbackSettings.verifyIntegrity),
  };
}

function inferOutputDir(outputPath) {
  const value = String(outputPath || "").trim();
  if (!value) return "";
  const normalized = value.replace(/[\\/]+$/g, "");
  const index = Math.max(normalized.lastIndexOf("\\"), normalized.lastIndexOf("/"));
  return index > 0 ? normalized.slice(0, index) : normalized;
}

function chunkMbFromBytes(value) {
  const bytes = Number(value || 0);
  if (!bytes) return 0;
  return Math.max(1, Math.round(bytes / (1024 * 1024)));
}

function taskMatchesFilter(task, filter) {
  if (filter === "active") return ACTIVE_STATUSES.has(task.status);
  if (filter === "paused") return task.status === "paused";
  if (filter === "completed") return task.status === "completed";
  if (filter === "failed") return task.status === "failed";
  if (filter === "cancelled") return task.status === "cancelled";
  return true;
}

function countFilters(tasks) {
  return {
    all: tasks.length,
    active: tasks.filter((task) => ACTIVE_STATUSES.has(task.status)).length,
    paused: tasks.filter((task) => task.status === "paused").length,
    completed: tasks.filter((task) => task.status === "completed").length,
    failed: tasks.filter((task) => task.status === "failed").length,
    cancelled: tasks.filter((task) => task.status === "cancelled").length,
  };
}

function extractMegaLinks(text) {
  const pattern = /(?:https?:\/\/)?(?:www\.)?mega(?:\.co)?\.nz\/[^\s"'<>]+/gi;
  const found = String(text || "").match(pattern) || [];
  const links = found.map(cleanLink).filter(Boolean);
  return Array.from(new Set(links));
}

function cleanLink(value) {
  let cleaned = String(value || "").trim().replace(/[)\],.;]+$/g, "");
  if (!cleaned) return "";
  if (!/^https?:\/\//i.test(cleaned)) cleaned = `https://${cleaned}`;
  return cleaned;
}

function readPassword(value) {
  const trimmed = value.trim();
  return trimmed ? trimmed : null;
}

function needsPassword(url) {
  const value = String(url || "").trim();
  if (!value) return false;
  if (value.includes("#P!")) return true;

  try {
    return new URL(value).hash.startsWith("#P!");
  } catch {
    return false;
  }
}

function formatEta(status, remainingBytes, speedBps) {
  if (status === "completed") return "已完成";
  if (status === "failed") return "失败";
  if (status === "cancelled") return "已取消";
  if (status === "paused") return "已暂停";
  if (!remainingBytes) return "计算中";
  if (!speedBps) return "计算中";
  return `剩余 ${formatDuration(remainingBytes / speedBps)}`;
}

function formatDuration(seconds) {
  const totalSeconds = Math.max(1, Math.ceil(Number(seconds || 0)));
  if (totalSeconds < 60) return `${totalSeconds} 秒`;

  const minutes = Math.floor(totalSeconds / 60);
  const restSeconds = totalSeconds % 60;
  if (minutes < 60) return `${minutes} 分 ${restSeconds} 秒`;

  const hours = Math.floor(minutes / 60);
  const restMinutes = minutes % 60;
  return `${hours} 小时 ${restMinutes} 分`;
}

function formatBytes(value) {
  const number = Number(value || 0);
  if (number < 1024) return `${number.toFixed(0)} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let size = number / 1024;
  let index = 0;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  return `${size >= 100 ? size.toFixed(0) : size.toFixed(1)} ${units[index]}`;
}

function formatTime(value) {
  const number = Number(value || 0);
  if (!number) return "-";
  return new Date(number).toLocaleString("zh-CN", { hour12: false });
}

createRoot(document.querySelector("#root")).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
