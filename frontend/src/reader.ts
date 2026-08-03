export {};

type Progress = {
  bookId: string;
  chapterId: string;
  blockId: string | null;
  chapterProgress: number;
  overallProgress: number;
  updatedAt: number;
};

type Chapter = {
  id: string;
  title: string;
  url: string;
  hasParallelText: boolean;
};

type Book = {
  id: string;
  title: string;
  author: string;
  language: string | null;
  coverUrl: string;
  entryUrl: string;
  chapters: Chapter[];
  progress: Progress | null;
};

type AnnotationKind = "highlight" | "note" | "bookmark";
type HighlightColor = "yellow" | "green" | "blue" | "pink";
type ReaderTheme = "system" | "light" | "dark";
type AiSendKey = "enter" | "mod-enter";

type AgentRuntime = {
  id: string;
  name: string;
  available: boolean;
  version: string | null;
  detail: string | null;
};

type AiMessage = {
  id: string;
  role: "user" | "assistant";
  content: string;
  runtimeId: string | null;
  createdAt: number;
  durationMs: number | null;
};

type AgentTask = {
  id: string;
  status: string;
  currentRuntimeId: string;
  error: string | null;
};

type BookAiWorkspace = {
  bookId: string;
  messages: AiMessage[];
  activeTasks: AgentTask[];
};

type AiViewState = {
  open: boolean;
  runtimeId: string;
  draft: string;
  timelineScrollTop: number | null;
  timelineFollowLatest: boolean;
  settingsOpen: boolean;
};

type AiWorkspaceCache = {
  workspace: BookAiWorkspace;
  runtimes: AgentRuntime[];
};

type Annotation = {
  id: string;
  bookId: string;
  chapterId: string;
  blockId: string;
  startOffset: number;
  endOffset: number;
  quote: string;
  kind: AnnotationKind;
  color: HighlightColor | null;
  note: string | null;
  createdAt: number;
  updatedAt: number;
};

type SelectionAnchor = {
  chapterId: string;
  blockId: string;
  startOffset: number;
  endOffset: number;
  quote: string;
  rect: DOMRect;
};

type ParallelText = {
  schemaVersion: number;
  language: string;
  blocks: Record<string, string>;
};

type HighlightRegistryLike = {
  clear(): void;
  set(name: string, highlight: unknown): void;
};

const runtimeScript = document.currentScript as HTMLScriptElement | null;
const bookId = runtimeScript?.dataset.goodreaderBook ?? "";
const chapterId = runtimeScript?.dataset.goodreaderChapter ?? "";
const content = document.querySelector<HTMLElement>("[data-goodreader-content]");

let book: Book;
let annotations: Annotation[] = [];
let currentSelection: SelectionAnchor | null = null;
let highlightColor: HighlightColor = "yellow";
let annotationFilter: AnnotationKind | "all" = "all";
let progressTimer: number | null = null;
let parallelCache: ParallelText | null | undefined;
let sidebarReturnFocus: HTMLElement | null = null;
let noteReturnFocus: HTMLElement | null = null;
let readerTheme: ReaderTheme = "system";
let aiSidebarReturnFocus: HTMLElement | null = null;
let aiWorkspace: BookAiWorkspace | null = null;
let aiRuntimes: AgentRuntime[] = [];
let aiPollTimer: number | null = null;
let topbarHideTimer: number | null = null;
let topbarRevealTimer: number | null = null;
let topbarPinned = false;
let aiSendKey: AiSendKey = "mod-enter";
let aiViewState: AiViewState = {
  open: false,
  runtimeId: "",
  draft: "",
  timelineScrollTop: null,
  timelineFollowLatest: true,
  settingsOpen: false,
};

const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");

const colorLabels: Record<HighlightColor, string> = {
  yellow: "明黄",
  green: "青绿",
  blue: "天蓝",
  pink: "粉红",
};

async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
  const method = options.method ?? "GET";
  const headers = new Headers(options.headers);
  if (!["GET", "HEAD"].includes(method)) headers.set("Content-Type", "application/json");
  const response = await fetch(path, { ...options, method, headers });
  if (!response.ok) {
    let message = `请求失败（${response.status}）`;
    try {
      const body = (await response.json()) as { error?: string };
      if (body.error) message = body.error;
    } catch {
      // 保留状态码错误。
    }
    throw new Error(message);
  }
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

function escapeHtml(value: string): string {
  return value.replace(
    /[&<>"']/g,
    (character) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" })[
        character
      ] ?? character,
  );
}

function readerIcon(name: string): string {
  const paths: Record<string, string> = {
    back: '<path d="m15 18-6-6 6-6"/>',
    previous: '<path d="m14 18-6-6 6-6"/>',
    next: '<path d="m10 6 6 6-6 6"/>',
    panel: '<path d="M4 5h16v14H4z"/><path d="M14 5v14M7 9h4M7 13h4"/>',
    close: '<path d="m6 6 12 12M18 6 6 18"/>',
    copy: '<rect x="8" y="8" width="11" height="11" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
    bookmark: '<path d="M6 4h12v17l-6-4-6 4V4Z"/>',
    note: '<path d="M5 4h14v13H9l-4 4V4Z"/><path d="M8 8h8M8 12h6"/>',
    check: '<path d="m5 12 4 4L19 6"/>',
    warning: '<path d="M12 3 2.8 20h18.4L12 3Z"/><path d="M12 9v5M12 17.5v.1"/>',
    globe: '<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.3 2.3 3.6 5.2 3.6 8.5s-1.3 6.2-3.6 8.5M12 3.5C9.7 5.8 8.4 8.7 8.4 12s1.3 6.2 3.6 8.5"/>',
    moon: '<path d="M20.5 15.2A8.5 8.5 0 0 1 8.8 3.5 8.7 8.7 0 1 0 20.5 15.2Z"/>',
    sun: '<circle cx="12" cy="12" r="3.5"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
    sparkles: '<path d="m12 3 1.4 4.1L17.5 8.5l-4.1 1.4L12 14l-1.4-4.1-4.1-1.4 4.1-1.4L12 3Z"/><path d="m19 14 .8 2.2L22 17l-2.2.8L19 20l-.8-2.2L16 17l2.2-.8L19 14Z"/>',
    pin: '<path d="M8 3h8l-1.5 6 2.5 3v2H7v-2l2.5-3L8 3Z"/><path d="M12 14v7"/>',
    settings: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1-2.8 2.8-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.6v.2h-4V21a1.7 1.7 0 0 0-1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1L4.2 17l.1-.1a1.7 1.7 0 0 0 .3-1.9A1.7 1.7 0 0 0 3 14H2.8v-4H3a1.7 1.7 0 0 0 1.6-1 1.7 1.7 0 0 0-.3-1.9L4.2 7 7 4.2l.1.1A1.7 1.7 0 0 0 9 4.6 1.7 1.7 0 0 0 10 3v-.2h4V3a1.7 1.7 0 0 0 1 1.6 1.7 1.7 0 0 0 1.9-.3l.1-.1L19.8 7l-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0 1.6 1h.2v4H21a1.7 1.7 0 0 0-1.6 1Z"/>',
  };
  return `<svg viewBox="0 0 24 24" aria-hidden="true">${paths[name] ?? paths.panel}</svg>`;
}

function effectiveTheme(): Exclude<ReaderTheme, "system"> {
  if (readerTheme === "system") return systemTheme.matches ? "dark" : "light";
  return readerTheme;
}

function applyReaderTheme(): void {
  const theme = effectiveTheme();
  document.documentElement.classList.add("gr-active");
  document.documentElement.dataset.goodreaderTheme = theme;
  document.documentElement.style.colorScheme = theme;

  const toggle = document.querySelector<HTMLButtonElement>(".gr-theme-toggle");
  if (!toggle) return;
  const nextTheme = theme === "dark" ? "明亮" : "黑暗";
  toggle.innerHTML = readerIcon(theme === "dark" ? "sun" : "moon");
  toggle.setAttribute("aria-label", `切换到${nextTheme}模式`);
  toggle.title = `切换到${nextTheme}模式`;
  toggle.setAttribute("aria-pressed", String(theme === "dark"));
}

async function toggleReaderTheme(): Promise<void> {
  readerTheme = effectiveTheme() === "dark" ? "light" : "dark";
  applyReaderTheme();
  try {
    await api<void>("/api/settings/reader-theme", {
      method: "PUT",
      body: JSON.stringify({ value: readerTheme }),
    });
    toast(readerTheme === "dark" ? "已开启黑暗模式" : "已开启明亮模式");
  } catch {
    toast("主题偏好暂未保存");
  }
}

function setTopbarVisible(visible: boolean): void {
  if (!visible && topbarPinned) return;
  document.body.classList.toggle("gr-topbar-hidden", !visible);
}

function clearTopbarTimer(timer: "hide" | "reveal"): void {
  const value = timer === "hide" ? topbarHideTimer : topbarRevealTimer;
  if (value !== null) window.clearTimeout(value);
  if (timer === "hide") topbarHideTimer = null;
  else topbarRevealTimer = null;
}

function scheduleTopbarHide(topbar: HTMLElement, delay: number): void {
  clearTopbarTimer("hide");
  if (topbarPinned) return;
  topbarHideTimer = window.setTimeout(() => {
    topbarHideTimer = null;
    if (topbar.matches(":hover") || topbar.contains(document.activeElement)) return;
    setTopbarVisible(false);
  }, delay);
}

function applyTopbarPinned(topbar: HTMLElement): void {
  document.body.classList.toggle("gr-topbar-pinned", topbarPinned);
  const button = topbar.querySelector<HTMLButtonElement>(".gr-topbar-pin");
  if (button) {
    button.setAttribute("aria-pressed", String(topbarPinned));
    button.setAttribute("aria-label", topbarPinned ? "取消固定顶部栏" : "固定顶部栏");
    button.title = topbarPinned ? "取消固定顶部栏" : "固定顶部栏";
  }
  if (topbarPinned) {
    clearTopbarTimer("hide");
    clearTopbarTimer("reveal");
    setTopbarVisible(true);
  } else {
    scheduleTopbarHide(topbar, 3000);
  }
}

function bindTopbarAutoHide(topbar: HTMLElement): void {
  const revealZone = document.createElement("div");
  revealZone.className = "gr-topbar-reveal-zone";
  revealZone.dataset.goodreaderUi = "true";
  revealZone.setAttribute("aria-hidden", "true");
  document.body.append(revealZone);

  const revealAfterDelay = () => {
    clearTopbarTimer("reveal");
    topbarRevealTimer = window.setTimeout(() => {
      topbarRevealTimer = null;
      setTopbarVisible(true);
    }, 500);
  };

  revealZone.addEventListener("pointerenter", revealAfterDelay);
  revealZone.addEventListener("pointerleave", () => clearTopbarTimer("reveal"));
  revealZone.addEventListener("pointerdown", () => {
    clearTopbarTimer("reveal");
    setTopbarVisible(true);
  });
  topbar.addEventListener("pointerenter", () => {
    clearTopbarTimer("hide");
    setTopbarVisible(true);
  });
  topbar.addEventListener("pointerleave", () => scheduleTopbarHide(topbar, 400));
  topbar.addEventListener("focusin", () => {
    clearTopbarTimer("hide");
    setTopbarVisible(true);
  });
  topbar.addEventListener("focusout", (event) => {
    if (!topbar.contains(event.relatedTarget as Node | null)) scheduleTopbarHide(topbar, 400);
  });

  setTopbarVisible(true);
  if (!topbarPinned) scheduleTopbarHide(topbar, 3000);
}

function injectChrome(): void {
  document.body.classList.add("gr-body");
  hideBookOwnedChrome();
  const chapterIndex = book.chapters.findIndex((chapter) => chapter.id === chapterId);
  const previous = chapterIndex > 0 ? book.chapters[chapterIndex - 1] : undefined;
  const next =
    chapterIndex >= 0 && chapterIndex < book.chapters.length - 1
      ? book.chapters[chapterIndex + 1]
      : undefined;

  const topbar = document.createElement("header");
  topbar.className = "gr-topbar";
  topbar.dataset.goodreaderUi = "true";
  topbar.innerHTML = `
    <div class="gr-topbar-leading">
      <button class="gr-back" type="button" aria-label="返回书架">
        ${readerIcon("back")}
        <span>书架</span>
      </button>
      ${
        chapterId
          ? `<div class="gr-chapter-navigation" aria-label="章节导航">
              <button type="button" data-chapter-nav="previous" aria-label="上一章"${previous ? "" : " disabled"}>${readerIcon("previous")}</button>
              <button type="button" data-chapter-nav="next" aria-label="下一章"${next ? "" : " disabled"}>${readerIcon("next")}</button>
            </div>`
          : ""
      }
      <button class="gr-topbar-pin" type="button" aria-label="固定顶部栏" title="固定顶部栏" aria-pressed="false">
        ${readerIcon("pin")}
      </button>
    </div>
    <div class="gr-title">
      <strong>${escapeHtml(currentChapter()?.title ?? "书籍首页")}</strong>
      <span>${escapeHtml(book.title)}</span>
    </div>
    <div class="gr-top-actions">
      ${chapterId ? `<span class="gr-progress-label" id="grProgressLabel">0%</span>` : ""}
      <button class="gr-ai-toggle" type="button" aria-label="打开书籍 AI 工作区" title="书籍 AI 工作区" aria-controls="grAiSidebar" aria-expanded="false">
        ${readerIcon("sparkles")}
      </button>
      <button class="gr-theme-toggle" type="button"></button>
      <button class="gr-sidebar-toggle" type="button" aria-label="打开目录与标注" aria-controls="grSidebar" aria-expanded="false">
        ${readerIcon("panel")}
      </button>
    </div>
    ${
      chapterId
        ? `<div class="gr-reading-progress" id="grReadingProgress" role="progressbar" aria-label="全书阅读进度" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0"><i></i></div>`
        : ""
    }
  `;
  document.body.append(topbar);
  bindTopbarAutoHide(topbar);
  applyTopbarPinned(topbar);
  topbar.querySelector(".gr-back")?.addEventListener("click", () => {
    window.location.href = "/";
  });
  topbar.querySelector('[data-chapter-nav="previous"]')?.addEventListener("click", () => {
    if (previous) window.location.href = previous.url;
  });
  topbar.querySelector('[data-chapter-nav="next"]')?.addEventListener("click", () => {
    if (next) window.location.href = next.url;
  });
  topbar.querySelector(".gr-topbar-pin")?.addEventListener("click", () => {
    topbarPinned = !topbarPinned;
    applyTopbarPinned(topbar);
    void savePreference("topbar-pinned", String(topbarPinned));
  });
  topbar.querySelector(".gr-sidebar-toggle")?.addEventListener("click", (event) => {
    const toggle = event.currentTarget as HTMLElement;
    const shouldOpen = toggle.getAttribute("aria-expanded") !== "true";
    if (shouldOpen) {
      sidebarReturnFocus = toggle;
      toggleAiSidebar(false, false);
    }
    toggleSidebar(shouldOpen);
  });
  topbar.querySelector(".gr-theme-toggle")?.addEventListener("click", () => {
    void toggleReaderTheme();
  });
  topbar.querySelector(".gr-ai-toggle")?.addEventListener("click", (event) => {
    const toggle = event.currentTarget as HTMLElement;
    const shouldOpen = toggle.getAttribute("aria-expanded") !== "true";
    if (shouldOpen) {
      aiSidebarReturnFocus = toggle;
      toggleSidebar(false);
    }
    toggleAiSidebar(shouldOpen);
  });
  applyReaderTheme();

  const overlay = document.createElement("div");
  overlay.className = "gr-sidebar-overlay";
  overlay.dataset.goodreaderUi = "true";
  overlay.addEventListener("click", () => toggleSidebar(false));
  document.body.append(overlay);

  const sidebar = document.createElement("aside");
  sidebar.className = "gr-sidebar";
  sidebar.id = "grSidebar";
  sidebar.dataset.goodreaderUi = "true";
  sidebar.setAttribute("aria-label", "目录与标注");
  sidebar.setAttribute("aria-hidden", "true");
  sidebar.innerHTML = `
    <div class="gr-sidebar-head">
      <div class="gr-sidebar-tabs" role="tablist" aria-label="阅读导航">
        <button class="active" data-tab="toc" role="tab" aria-selected="true" aria-controls="grTocPane">目录</button>
        <button data-tab="annotations" role="tab" aria-selected="false" aria-controls="grAnnotationsPane">标注</button>
      </div>
      <button class="gr-close" aria-label="关闭目录与标注">${readerIcon("close")}</button>
    </div>
    <div class="gr-sidebar-body">
      <section class="gr-pane active" id="grTocPane" data-pane="toc" role="tabpanel"></section>
      <section class="gr-pane" id="grAnnotationsPane" data-pane="annotations" role="tabpanel" hidden></section>
    </div>
  `;
  document.body.append(sidebar);
  sidebar.querySelector(".gr-close")?.addEventListener("click", () => toggleSidebar(false));
  sidebar.querySelectorAll<HTMLElement>("[data-tab]").forEach((button) => {
    button.addEventListener("click", () => {
      sidebar.querySelectorAll<HTMLElement>("[data-tab]").forEach((item) => {
        item.classList.remove("active");
        item.setAttribute("aria-selected", "false");
      });
      sidebar.querySelectorAll<HTMLElement>("[data-pane]").forEach((item) => {
        item.classList.remove("active");
        item.hidden = true;
      });
      button.classList.add("active");
      button.setAttribute("aria-selected", "true");
      const pane = sidebar.querySelector<HTMLElement>(`[data-pane="${button.dataset.tab}"]`);
      pane?.classList.add("active");
      if (pane) pane.hidden = false;
    });
  });

  injectAiSidebar();

  renderToc();
  renderAnnotations();
  document.addEventListener("keydown", handleReaderKeyboard);
}

function aiStateKey(): string {
  return `goodreader-ai-view:${bookId}`;
}

function aiWorkspaceCacheKey(): string {
  return `goodreader-ai-workspace:${bookId}`;
}

function restoreAiWorkspaceCache(): void {
  try {
    const cached = JSON.parse(sessionStorage.getItem(aiWorkspaceCacheKey()) ?? "null") as AiWorkspaceCache | null;
    if (!cached || cached.workspace.bookId !== bookId || !Array.isArray(cached.runtimes)) return;
    aiWorkspace = cached.workspace;
    aiRuntimes = cached.runtimes;
  } catch {
    // 缓存损坏或超出容量时，退回实时加载。
  }
}

function saveAiWorkspaceCache(): void {
  if (!aiWorkspace) return;
  try {
    sessionStorage.setItem(
      aiWorkspaceCacheKey(),
      JSON.stringify({ workspace: aiWorkspace, runtimes: aiRuntimes } satisfies AiWorkspaceCache),
    );
  } catch {
    // 对话过长时不缓存，不影响实时工作区。
  }
}

function restoreAiViewState(): AiViewState {
  try {
    const value = JSON.parse(sessionStorage.getItem(aiStateKey()) ?? "{}") as Partial<AiViewState>;
    return {
      open: value.open === true,
      runtimeId: typeof value.runtimeId === "string" ? value.runtimeId : "",
      draft: typeof value.draft === "string" ? value.draft : "",
      timelineScrollTop:
        typeof value.timelineScrollTop === "number" && value.timelineScrollTop >= 0
          ? value.timelineScrollTop
          : null,
      timelineFollowLatest: value.timelineFollowLatest !== false,
      settingsOpen: value.settingsOpen === true,
    };
  } catch {
    return {
      open: false,
      runtimeId: "",
      draft: "",
      timelineScrollTop: null,
      timelineFollowLatest: true,
      settingsOpen: false,
    };
  }
}

function saveAiViewState(): void {
  sessionStorage.setItem(aiStateKey(), JSON.stringify(aiViewState));
}

function injectAiSidebar(): void {
  const overlay = document.createElement("div");
  overlay.className = "gr-ai-overlay";
  overlay.dataset.goodreaderUi = "true";
  overlay.addEventListener("click", () => toggleAiSidebar(false));
  document.body.append(overlay);

  const sidebar = document.createElement("aside");
  sidebar.className = "gr-ai-sidebar";
  sidebar.id = "grAiSidebar";
  sidebar.dataset.goodreaderUi = "true";
  sidebar.setAttribute("aria-label", "书籍 AI 工作区");
  sidebar.setAttribute("aria-hidden", "true");
  sidebar.innerHTML = `<div class="gr-ai-loading" role="status">${readerIcon("sparkles")}<strong>正在打开书籍 AI</strong><span>恢复共享历史与后台任务…</span></div>`;
  document.body.append(sidebar);

  if (aiViewState.open) toggleAiSidebar(true, false);
}

function toggleAiSidebar(open: boolean, focus = true): void {
  const sidebar = document.querySelector<HTMLElement>("#grAiSidebar");
  if (!sidebar) return;
  aiViewState.open = open;
  saveAiViewState();
  document.body.classList.toggle("gr-ai-open", open);
  sidebar.classList.toggle("open", open);
  sidebar.setAttribute("aria-hidden", String(!open));
  document.querySelector(".gr-ai-overlay")?.classList.toggle("open", open);
  const toggle = document.querySelector<HTMLElement>(".gr-ai-toggle");
  toggle?.setAttribute("aria-expanded", String(open));
  toggle?.setAttribute("aria-pressed", String(open));
  if (open) {
    if (aiWorkspace) {
      renderAiSidebar();
      void refreshAiWorkspace();
    } else {
      void refreshAiWorkspace();
    }
    if (focus) {
      window.setTimeout(() => sidebar.querySelector<HTMLElement>(".gr-ai-close, select, textarea")?.focus());
    }
  } else if (focus) {
    aiSidebarReturnFocus?.focus();
    aiSidebarReturnFocus = null;
  }
}

async function refreshAiWorkspace(): Promise<void> {
  try {
    const [workspace, runtimes] = await Promise.all([
      api<BookAiWorkspace>(`/api/books/${encodeURIComponent(bookId)}/ai`),
      api<AgentRuntime[]>("/api/agent/runtimes"),
    ]);
    aiWorkspace = workspace;
    aiRuntimes = runtimes;
    saveAiWorkspaceCache();
    const available = runtimes.filter((runtime) => runtime.available);
    const activeRuntime = workspace.activeTasks[0]?.currentRuntimeId;
    if (!available.some((runtime) => runtime.id === aiViewState.runtimeId)) {
      aiViewState.runtimeId = available.some((runtime) => runtime.id === activeRuntime)
        ? activeRuntime ?? ""
        : available[0]?.id ?? "";
      saveAiViewState();
    }
    renderAiSidebar();
    const activeTask = workspace.activeTasks[0];
    if (activeTask && ["queued", "running"].includes(activeTask.status)) {
      pollAiTask(activeTask.id);
    }
  } catch (error) {
    const sidebar = document.querySelector<HTMLElement>("#grAiSidebar");
    if (sidebar && !aiWorkspace) {
      sidebar.innerHTML = `<div class="gr-ai-error">${readerIcon("warning")}<strong>AI 工作区暂时无法打开</strong><p>${escapeHtml(error instanceof Error ? error.message : "未知错误")}</p><button type="button">重试</button></div>`;
      sidebar.querySelector("button")?.addEventListener("click", () => void refreshAiWorkspace());
    } else {
      toast(error instanceof Error ? error.message : "无法刷新 AI 工作区");
    }
  }
}

function renderAiSidebar(): void {
  const sidebar = document.querySelector<HTMLElement>("#grAiSidebar");
  if (!sidebar || !aiWorkspace) return;
  const oldTimeline = sidebar.querySelector<HTMLElement>("#grAiTimeline");
  if (oldTimeline) rememberAiTimelinePosition(oldTimeline);
  const followLatest = aiViewState.timelineFollowLatest;
  const oldScrollTop = aiViewState.timelineScrollTop ?? 0;
  const available = aiRuntimes.filter((runtime) => runtime.available);
  const activeTask = aiWorkspace.activeTasks[0];
  sidebar.innerHTML = `
    <header class="gr-ai-head">
      <div class="gr-ai-title">
        <span>${readerIcon("sparkles")}</span>
        <div><strong>书籍 AI</strong><small>${escapeHtml(book.title)}</small></div>
      </div>
      <div class="gr-ai-head-actions">
        <button class="gr-ai-clear" type="button"${activeTask ? " disabled" : ""}>清除</button>
        <button class="gr-ai-close" type="button" aria-label="关闭书籍 AI">${readerIcon("close")}</button>
      </div>
    </header>
    <main class="gr-ai-timeline" id="grAiTimeline">
      ${
        aiWorkspace.messages.length
          ? aiWorkspace.messages.map(aiMessageHtml).join("")
          : `<div class="gr-ai-empty">${readerIcon("sparkles")}<strong>边读边问</strong><p>可以总结当前书籍、追问概念，或整理你的标注。</p><button type="button" data-ai-suggestion="总结这本书的核心观点，并给出对应章节引用。">总结全书</button></div>`
      }
    </main>
    <section class="gr-ai-settings" id="grAiSettings" aria-label="AI 工作区设置"${aiViewState.settingsOpen ? "" : " hidden"}>
      <label for="grAiRuntime"><span>Agent</span><select id="grAiRuntime"${available.length ? "" : " disabled"}>
        ${aiRuntimes.map((runtime) => `<option value="${escapeHtml(runtime.id)}"${runtime.id === aiViewState.runtimeId ? " selected" : ""}${runtime.available ? "" : " disabled"}>${escapeHtml(runtime.name)}${runtime.available && runtime.version ? ` · ${escapeHtml(runtime.version)}` : runtime.available ? "" : " · 不可用"}</option>`).join("")}
      </select></label>
      <label for="grAiSendKey"><span>发送键</span><select id="grAiSendKey">
        <option value="mod-enter"${aiSendKey === "mod-enter" ? " selected" : ""}>⌘ Enter 发送</option>
        <option value="enter"${aiSendKey === "enter" ? " selected" : ""}>Enter 发送 · Shift Enter 换行</option>
      </select></label>
    </section>
    <div class="gr-ai-task${activeTask ? " visible" : ""}" id="grAiTask" role="status" aria-live="polite">${activeTask ? aiTaskHtml(activeTask) : ""}</div>
    <footer class="gr-ai-composer">
      <label for="grAiQuestion">问这本书</label>
      <textarea id="grAiQuestion" rows="3" placeholder="输入问题…"${available.length && !activeTask ? "" : " disabled"}>${escapeHtml(aiViewState.draft)}</textarea>
      <div>
        <button class="gr-ai-settings-toggle" type="button" aria-label="AI 工作区设置" title="设置" aria-controls="grAiSettings" aria-expanded="${String(aiViewState.settingsOpen)}">${readerIcon("settings")}</button>
        <button class="gr-ai-send" type="button"${available.length && !activeTask ? "" : " disabled"}>${readerIcon("sparkles")}<span>发送</span></button>
      </div>
    </footer>
  `;

  const timeline = sidebar.querySelector<HTMLElement>("#grAiTimeline");
  if (timeline) {
    timeline.scrollTop = followLatest ? timeline.scrollHeight : oldScrollTop;
    rememberAiTimelinePosition(timeline);
    timeline.addEventListener("scroll", () => rememberAiTimelinePosition(timeline), {
      passive: true,
    });
  }
  sidebar.querySelector<HTMLButtonElement>(".gr-ai-close")?.addEventListener("click", () => toggleAiSidebar(false));
  sidebar.querySelector<HTMLSelectElement>("#grAiRuntime")?.addEventListener("change", (event) => {
    aiViewState.runtimeId = (event.currentTarget as HTMLSelectElement).value;
    saveAiViewState();
  });
  sidebar.querySelector<HTMLSelectElement>("#grAiSendKey")?.addEventListener("change", (event) => {
    const value = (event.currentTarget as HTMLSelectElement).value;
    if (value !== "enter" && value !== "mod-enter") return;
    aiSendKey = value;
    void savePreference("ai-send-key", value);
  });
  sidebar.querySelector<HTMLButtonElement>(".gr-ai-settings-toggle")?.addEventListener("click", (event) => {
    aiViewState.settingsOpen = !aiViewState.settingsOpen;
    saveAiViewState();
    const settings = sidebar.querySelector<HTMLElement>("#grAiSettings");
    if (settings) settings.hidden = !aiViewState.settingsOpen;
    const button = event.currentTarget as HTMLButtonElement;
    button.setAttribute("aria-expanded", String(aiViewState.settingsOpen));
    if (aiViewState.settingsOpen) settings?.querySelector<HTMLSelectElement>("select")?.focus();
  });
  const textarea = sidebar.querySelector<HTMLTextAreaElement>("#grAiQuestion");
  textarea?.addEventListener("input", () => {
    aiViewState.draft = textarea.value;
    saveAiViewState();
  });
  textarea?.addEventListener("keydown", (event) => {
    if (event.isComposing || event.key !== "Enter") return;
    const shouldSend =
      aiSendKey === "enter"
        ? !event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey
        : event.metaKey || event.ctrlKey;
    if (shouldSend) {
      event.preventDefault();
      sidebar.querySelector<HTMLButtonElement>(".gr-ai-send")?.click();
    }
  });
  sidebar.querySelector<HTMLButtonElement>(".gr-ai-send")?.addEventListener("click", (event) => {
    void submitAiQuestion(textarea, event.currentTarget as HTMLButtonElement);
  });
  sidebar.querySelector<HTMLButtonElement>("[data-ai-suggestion]")?.addEventListener("click", (event) => {
    if (!textarea) return;
    textarea.value = (event.currentTarget as HTMLButtonElement).dataset.aiSuggestion ?? "";
    aiViewState.draft = textarea.value;
    saveAiViewState();
    textarea.focus();
  });
  sidebar.querySelector<HTMLButtonElement>(".gr-ai-retry")?.addEventListener("click", (event) => {
    if (activeTask) void retryAiTask(activeTask, event.currentTarget as HTMLButtonElement);
  });
  sidebar.querySelector<HTMLButtonElement>(".gr-ai-clear")?.addEventListener("click", () => void clearAiWorkspace());
  sidebar.querySelectorAll<HTMLButtonElement>("[data-ai-chapter]").forEach((button) => {
    button.addEventListener("click", () => {
      const chapter = book.chapters.find((item) => item.id === button.dataset.aiChapter);
      if (!chapter) return;
      const blockId = button.dataset.aiBlock ?? "";
      if (chapter.id === chapterId) {
        const target = content?.querySelector<HTMLElement>(
          `[data-goodreader-block="${CSS.escape(blockId)}"]`,
        );
        if (!target) {
          toast("未找到引用位置");
          return;
        }
        const url = new URL(window.location.href);
        url.searchParams.set("block", blockId);
        window.history.replaceState(window.history.state, "", url);
        target.scrollIntoView({
          behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth",
          block: "center",
        });
        return;
      }
      aiViewState.open = true;
      saveAiViewState();
      saveAiWorkspaceCache();
      const url = new URL(chapter.url, window.location.origin);
      url.searchParams.set("block", blockId);
      window.location.href = `${url.pathname}${url.search}${url.hash}`;
    });
  });
}

function rememberAiTimelinePosition(timeline: HTMLElement): void {
  aiViewState.timelineScrollTop = timeline.scrollTop;
  aiViewState.timelineFollowLatest =
    timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 24;
  saveAiViewState();
}

function aiMessageHtml(message: AiMessage): string {
  const duration =
    message.role === "assistant" && message.durationMs !== null
      ? `<footer>耗时 ${formatDuration(message.durationMs)}</footer>`
      : "";
  return `<article class="gr-ai-message ${message.role}"><p>${renderAiContent(message.content)}</p>${duration}</article>`;
}

function formatDuration(milliseconds: number): string {
  const seconds = Math.max(1, Math.round(milliseconds / 1000));
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  const remaining = seconds % 60;
  return remaining ? `${minutes} 分 ${remaining} 秒` : `${minutes} 分钟`;
}

function renderAiContent(value: string): string {
  return escapeHtml(value)
    .replace(/\[chapter:([A-Za-z0-9._:-]+)#([A-Za-z0-9._:-]+)\]/g, (_match, chapter: string, block: string) => `<button class="gr-ai-citation" type="button" data-ai-chapter="${chapter}" data-ai-block="${block}">${readerIcon("bookmark")}<span>${escapeHtml(block)}</span></button>`)
    .replace(/\n/g, "<br />");
}

function aiTaskHtml(task: AgentTask): string {
  if (task.status === "paused") {
    return `${readerIcon("warning")}<span><strong>任务已暂停</strong>${escapeHtml(task.error ?? "Agent 暂时无法继续")}</span><button class="gr-ai-retry" type="button">切换后重试</button>`;
  }
  const runtime = aiRuntimes.find((item) => item.id === task.currentRuntimeId);
  return `<i></i><span><strong>${escapeHtml(runtime?.name ?? task.currentRuntimeId)} 正在处理</strong>关闭侧栏不会停止任务</span>`;
}

async function submitAiQuestion(textarea: HTMLTextAreaElement | null, button: HTMLButtonElement): Promise<void> {
  const content = textarea?.value.trim() ?? "";
  if (!content || !aiViewState.runtimeId) return;
  button.disabled = true;
  try {
    const task = await api<AgentTask>(`/api/books/${encodeURIComponent(bookId)}/ai/questions`, {
      method: "POST",
      body: JSON.stringify({ content, runtimeId: aiViewState.runtimeId }),
    });
    aiViewState.draft = "";
    saveAiViewState();
    await refreshAiWorkspace();
    pollAiTask(task.id);
  } catch (error) {
    toast(error instanceof Error ? error.message : "无法提交问题");
    if (button.isConnected) button.disabled = false;
  }
}

async function retryAiTask(task: AgentTask, button: HTMLButtonElement): Promise<void> {
  if (!aiViewState.runtimeId) return;
  button.disabled = true;
  try {
    const retried = await api<AgentTask>(`/api/agent/tasks/${encodeURIComponent(task.id)}/retry`, {
      method: "POST",
      body: JSON.stringify({ runtimeId: aiViewState.runtimeId }),
    });
    await refreshAiWorkspace();
    pollAiTask(retried.id);
  } catch (error) {
    toast(error instanceof Error ? error.message : "无法重试任务");
    if (button.isConnected) button.disabled = false;
  }
}

function pollAiTask(taskId: string): void {
  if (aiPollTimer !== null) window.clearTimeout(aiPollTimer);
  const poll = async () => {
    try {
      const task = await api<AgentTask>(`/api/agent/tasks/${encodeURIComponent(taskId)}`);
      if (["completed", "paused", "stopped"].includes(task.status)) {
        await refreshAiWorkspace();
        return;
      }
      const state = document.querySelector<HTMLElement>("#grAiTask");
      if (state) {
        state.classList.add("visible");
        state.innerHTML = aiTaskHtml(task);
      }
      aiPollTimer = window.setTimeout(poll, 1000);
    } catch (error) {
      toast(error instanceof Error ? error.message : "无法读取 Agent 任务状态");
    }
  };
  aiPollTimer = window.setTimeout(poll, 500);
}

async function clearAiWorkspace(): Promise<void> {
  if (!window.confirm(`清除《${book.title}》的 AI 对话历史？阅读进度和标注不会受到影响。`)) return;
  try {
    await api<void>(`/api/books/${encodeURIComponent(bookId)}/ai`, { method: "DELETE", body: "{}" });
    aiWorkspace = { bookId, messages: [], activeTasks: [] };
    saveAiWorkspaceCache();
    renderAiSidebar();
    toast("AI 工作区已清除");
  } catch (error) {
    toast(error instanceof Error ? error.message : "无法清除 AI 工作区");
  }
}

function currentChapter(): Chapter | undefined {
  return book.chapters.find((chapter) => chapter.id === chapterId);
}

function toggleSidebar(open: boolean): void {
  const sidebar = document.querySelector<HTMLElement>("#grSidebar");
  document.body.classList.toggle("gr-sidebar-open", open);
  sidebar?.classList.toggle("open", open);
  sidebar?.setAttribute("aria-hidden", String(!open));
  document.querySelector(".gr-sidebar-overlay")?.classList.toggle("open", open);
  const toggle = document.querySelector<HTMLElement>(".gr-sidebar-toggle");
  toggle?.setAttribute("aria-expanded", String(open));
  if (open) {
    window.setTimeout(() => {
      sidebar?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]')?.focus();
    });
  } else {
    sidebarReturnFocus?.focus();
    sidebarReturnFocus = null;
  }
}

function hideBookOwnedChrome(): void {
  if (!content) return;
  let branch: HTMLElement = content;
  while (branch.parentElement && branch.parentElement !== document.body) {
    const parent = branch.parentElement;
    for (const sibling of parent.children) {
      if (sibling !== branch && sibling instanceof HTMLElement) {
        sibling.dataset.goodreaderSourceUiHidden = "true";
      }
    }
    parent.dataset.goodreaderContentShell = "true";
    branch = parent;
  }
  if (branch.parentElement === document.body) {
    for (const sibling of document.body.children) {
      if (sibling !== branch && sibling instanceof HTMLElement) {
        sibling.dataset.goodreaderSourceUiHidden = "true";
      }
    }
  }
}

function removeParallelCard(): void {
  document.querySelector(".gr-parallel-card")?.remove();
}

function handleReaderKeyboard(event: KeyboardEvent): void {
  if (event.key !== "Escape") return;
  const noteModal = document.querySelector(".gr-note-modal");
  if (noteModal) {
    closeNoteEditor();
    return;
  }
  if (document.querySelector(".gr-parallel-card")) {
    removeParallelCard();
    return;
  }
  if (document.querySelector(".gr-selection-toolbar")) {
    hideToolbar();
    return;
  }
  if (document.querySelector("#grSidebar.open")) {
    toggleSidebar(false);
    return;
  }
  if (document.querySelector("#grAiSidebar.open")) toggleAiSidebar(false);
}

function renderToc(): void {
  const pane = document.querySelector<HTMLElement>('[data-pane="toc"]');
  if (!pane) return;
  pane.innerHTML = `
    <div class="gr-book-overview">
      <img src="${escapeHtml(book.coverUrl)}" alt="${escapeHtml(book.title)}封面" />
      <div>
        <strong>${escapeHtml(book.title)}</strong>
        <span>${escapeHtml(book.author)}</span>
        <small>${book.chapters.length} 章</small>
      </div>
    </div>
    <nav class="gr-toc" aria-label="章节">
      ${book.chapters
        .map(
          (chapter, index) => `
            <a href="${escapeHtml(chapter.url)}" class="${chapter.id === chapterId ? "current" : ""}"${chapter.id === chapterId ? ' aria-current="page"' : ""}>
              <span>${String(index + 1).padStart(2, "0")}</span>
              <strong>${escapeHtml(chapter.title)}</strong>
            </a>
          `,
        )
        .join("")}
    </nav>
  `;
}

function renderAnnotations(): void {
  const pane = document.querySelector<HTMLElement>('[data-pane="annotations"]');
  if (!pane) return;
  const filtered = annotations.filter(
    (annotation) => annotationFilter === "all" || annotation.kind === annotationFilter,
  );
  const counts: Record<AnnotationKind | "all", number> = {
    all: annotations.length,
    highlight: annotations.filter((annotation) => annotation.kind === "highlight").length,
    note: annotations.filter((annotation) => annotation.kind === "note").length,
    bookmark: annotations.filter((annotation) => annotation.kind === "bookmark").length,
  };
  pane.innerHTML = `
    <div class="gr-filter" aria-label="标注筛选">
      ${(["all", "highlight", "note", "bookmark"] as const)
        .map(
          (filter) =>
            `<button class="${filter === annotationFilter ? "active" : ""}" data-filter="${filter}" aria-pressed="${filter === annotationFilter}"><span>${{
              all: "全部",
              highlight: "高亮",
              note: "笔记",
              bookmark: "书签",
            }[filter]}</span><strong>${counts[filter]}</strong></button>`,
        )
        .join("")}
    </div>
    <div class="gr-annotation-list">
      ${
        filtered.length
          ? filtered.map(annotationCard).join("")
          : `<div class="gr-annotation-empty">${readerIcon("note")}<strong>还没有${annotationFilter === "all" ? "标注" : { highlight: "高亮", note: "笔记", bookmark: "书签" }[annotationFilter]}</strong><p>在正文中选中文字即可添加。</p></div>`
      }
    </div>
  `;
  pane.querySelectorAll<HTMLElement>("[data-filter]").forEach((button) => {
    button.addEventListener("click", () => {
      annotationFilter = button.dataset.filter as typeof annotationFilter;
      void api<void>("/api/settings/annotation-filter", {
        method: "PUT",
        body: JSON.stringify({ value: annotationFilter }),
      });
      renderAnnotations();
    });
  });
  pane.querySelectorAll<HTMLElement>("[data-annotation-id]").forEach((card) => {
    const annotation = annotations.find((item) => item.id === card.dataset.annotationId);
    if (!annotation) return;
    card.querySelector<HTMLElement>("[data-jump]")?.addEventListener("click", () => {
      jumpToAnnotation(annotation);
    });
    card.querySelector<HTMLElement>("[data-edit]")?.addEventListener("click", () => {
      openNoteEditor(annotation);
    });
    card.querySelector<HTMLElement>("[data-delete]")?.addEventListener("click", () => {
      confirmDeleteAnnotation(annotation);
    });
  });
}

function annotationCard(annotation: Annotation): string {
  const chapter = book.chapters.find((item) => item.id === annotation.chapterId);
  const type = {
    highlight: annotation.color ? colorLabels[annotation.color] : "高亮",
    note: "笔记",
    bookmark: "书签",
  }[annotation.kind];
  return `
    <article class="gr-annotation-card ${annotation.kind}" data-annotation-id="${escapeHtml(annotation.id)}">
      <button class="gr-annotation-main" data-jump>
        <span class="gr-kind ${annotation.color ?? ""}">${escapeHtml(type)}</span>
        <blockquote>${escapeHtml(annotation.quote)}</blockquote>
        ${annotation.note ? `<p>${escapeHtml(annotation.note)}</p>` : ""}
        <small>${escapeHtml(chapter?.title ?? annotation.chapterId)} · ${new Date(annotation.createdAt).toLocaleDateString("zh-CN")}</small>
      </button>
      <div class="gr-card-actions">
        ${annotation.kind === "note" ? `<button data-edit aria-label="编辑笔记">编辑</button>` : ""}
        <button data-delete aria-label="删除标注">删除</button>
      </div>
    </article>
  `;
}

function bindSelection(): void {
  if (!content) return;
  document.addEventListener("mouseup", (event) => {
    if ((event.target as Element | null)?.closest("[data-goodreader-ui]")) return;
    window.setTimeout(readSelection, 0);
  });
  document.addEventListener("keyup", (event) => {
    if (event.key === "Escape") hideToolbar();
    if (event.shiftKey || event.key.startsWith("Arrow")) window.setTimeout(readSelection, 0);
  });
  document.addEventListener("mousedown", (event) => {
    const target = event.target as Element;
    if (!target.closest(".gr-selection-toolbar") && !target.closest(".gr-note-modal")) {
      hideToolbar();
    }
  });
  document.addEventListener(
    "pointerdown",
    (event) => {
      const card = document.querySelector<HTMLElement>(".gr-parallel-card");
      if (card && event.target instanceof Node && !card.contains(event.target)) {
        removeParallelCard();
      }
    },
    true,
  );
}

function readSelection(): void {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0 || !content) {
    hideToolbar();
    return;
  }
  const range = selection.getRangeAt(0);
  const startElement = parentElement(range.startContainer);
  const endElement = parentElement(range.endContainer);
  const startBlock = startElement?.closest<HTMLElement>("[data-goodreader-block]");
  const endBlock = endElement?.closest<HTMLElement>("[data-goodreader-block]");
  if (!startBlock || startBlock !== endBlock || !content.contains(startBlock)) {
    hideToolbar();
    toast("请选择同一正文块内的内容");
    return;
  }

  const quote = range.toString();
  if (!quote.trim()) {
    hideToolbar();
    return;
  }
  const startOffset = offsetInBlock(startBlock, range.startContainer, range.startOffset);
  const endOffset = offsetInBlock(startBlock, range.endContainer, range.endOffset);
  if (endOffset <= startOffset) {
    hideToolbar();
    return;
  }
  currentSelection = {
    chapterId,
    blockId: startBlock.dataset.goodreaderBlock ?? "",
    startOffset,
    endOffset,
    quote,
    rect: range.getBoundingClientRect(),
  };
  showToolbar();
}

function parentElement(node: Node): Element | null {
  return node.nodeType === Node.ELEMENT_NODE ? (node as Element) : node.parentElement;
}

function textNodes(block: HTMLElement): Text[] {
  const nodes: Text[] = [];
  const walker = document.createTreeWalker(block, NodeFilter.SHOW_TEXT);
  let node: Node | null;
  while ((node = walker.nextNode())) nodes.push(node as Text);
  return nodes;
}

function offsetInBlock(block: HTMLElement, target: Node, offset: number): number {
  let total = 0;
  for (const node of textNodes(block)) {
    if (node === target) return total + offset;
    total += node.length;
  }
  return total;
}

function rangeFromAnnotation(annotation: Annotation): Range | null {
  const block = document.querySelector<HTMLElement>(
    `[data-goodreader-block="${CSS.escape(annotation.blockId)}"]`,
  );
  if (!block) return null;
  const range = document.createRange();
  let total = 0;
  let started = false;
  for (const node of textNodes(block)) {
    const next = total + node.length;
    if (!started && annotation.startOffset <= next) {
      range.setStart(node, Math.max(0, annotation.startOffset - total));
      started = true;
    }
    if (started && annotation.endOffset <= next) {
      range.setEnd(node, Math.max(0, annotation.endOffset - total));
      return range;
    }
    total = next;
  }
  return null;
}

function showToolbar(): void {
  hideToolbar();
  if (!currentSelection) return;
  const toolbar = document.createElement("div");
  toolbar.className = "gr-selection-toolbar";
  toolbar.dataset.goodreaderUi = "true";
  toolbar.setAttribute("role", "toolbar");
  toolbar.setAttribute("aria-label", "文字标注工具");
  toolbar.innerHTML = `
    <button type="button" data-action="copy" aria-label="复制所选文字" title="复制">
      ${readerIcon("copy")}
    </button>
    <span class="gr-toolbar-divider"></span>
    ${(["yellow", "green", "blue", "pink"] as HighlightColor[])
      .map(
        (color) =>
          `<button type="button" class="gr-color ${color} ${highlightColor === color ? "selected" : ""}" data-highlight="${color}" aria-label="添加${colorLabels[color]}高亮" aria-pressed="${highlightColor === color}" title="${colorLabels[color]}高亮"><span></span></button>`,
      )
      .join("")}
    <span class="gr-toolbar-divider"></span>
    <button type="button" data-action="bookmark" aria-label="添加书签" title="书签">
      ${readerIcon("bookmark")}
    </button>
    <button type="button" data-action="note" aria-label="添加笔记" title="笔记">
      ${readerIcon("note")}
    </button>
    <button type="button" data-action="parallel" aria-label="当前内容没有可用原文" title="当前内容没有可用原文" disabled>${readerIcon("globe")}</button>
  `;
  document.body.append(toolbar);
  const rect = currentSelection.rect;
  const toolbarRect = toolbar.getBoundingClientRect();
  const left = Math.max(
    10,
    Math.min(window.innerWidth - toolbarRect.width - 10, rect.left + rect.width / 2 - toolbarRect.width / 2),
  );
  const above = rect.top - toolbarRect.height - 12;
  toolbar.style.left = `${left}px`;
  toolbar.style.top = `${above > 8 ? above : rect.bottom + 12}px`;

  toolbar.querySelector('[data-action="copy"]')?.addEventListener("click", () => {
    if (!currentSelection) return;
    void navigator.clipboard.writeText(currentSelection.quote);
    toast("已复制");
    hideToolbar();
  });
  toolbar.querySelectorAll<HTMLElement>("[data-highlight]").forEach((button) => {
    button.addEventListener("click", () => {
      const color = button.dataset.highlight as HighlightColor;
      highlightColor = color;
      void api<void>("/api/settings/highlight-color", {
        method: "PUT",
        body: JSON.stringify({ value: color }),
      });
      void createAnnotation("highlight", color);
    });
  });
  toolbar.querySelector('[data-action="bookmark"]')?.addEventListener("click", () => {
    void createAnnotation("bookmark");
  });
  toolbar.querySelector('[data-action="note"]')?.addEventListener("click", () => {
    openNoteEditor();
  });
  toolbar.querySelector('[data-action="parallel"]')?.addEventListener("click", () => {
    void showParallelText();
  });
  void updateParallelButton(toolbar, currentSelection.blockId);
}

async function updateParallelButton(toolbar: HTMLElement, blockId: string): Promise<void> {
  const button = toolbar.querySelector<HTMLButtonElement>('[data-action="parallel"]');
  if (!button || !currentChapter()?.hasParallelText) return;
  try {
    if (parallelCache === undefined) {
      parallelCache = await api<ParallelText>(
        `/api/books/${encodeURIComponent(bookId)}/parallel/${encodeURIComponent(chapterId)}`,
      );
    }
    if (!toolbar.isConnected || currentSelection?.blockId !== blockId) return;
    if (parallelCache?.blocks[blockId]) {
      button.disabled = false;
      button.title = "显示原文";
      button.setAttribute("aria-label", "显示所选正文块原文");
    }
  } catch {
    // 原文入口保持可见但置灰，阅读和标注功能不受影响。
  }
}

function hideToolbar(): void {
  document.querySelector(".gr-selection-toolbar")?.remove();
}

async function createAnnotation(
  kind: AnnotationKind,
  color?: HighlightColor,
  note?: string,
): Promise<void> {
  if (!currentSelection) return;
  try {
    const annotation = await api<Annotation>(
      `/api/books/${encodeURIComponent(bookId)}/annotations`,
      {
        method: "POST",
        body: JSON.stringify({
          chapterId: currentSelection.chapterId,
          blockId: currentSelection.blockId,
          startOffset: currentSelection.startOffset,
          endOffset: currentSelection.endOffset,
          quote: currentSelection.quote,
          kind,
          color: color ?? null,
          note: note ?? null,
        }),
      },
    );
    const existing = annotations.findIndex((item) => item.id === annotation.id);
    if (existing >= 0) annotations[existing] = annotation;
    else annotations.unshift(annotation);
    renderHighlights();
    renderAnnotations();
    hideToolbar();
    window.getSelection()?.removeAllRanges();
    toast({ highlight: "高亮已添加", note: "笔记已保存", bookmark: "书签已添加" }[kind]);
  } catch (error) {
    toast(error instanceof Error ? error.message : "标注失败");
  }
}

function openNoteEditor(annotation?: Annotation): void {
  const anchor = annotation
    ? {
        chapterId: annotation.chapterId,
        blockId: annotation.blockId,
        startOffset: annotation.startOffset,
        endOffset: annotation.endOffset,
        quote: annotation.quote,
        rect: new DOMRect(),
      }
    : currentSelection;
  if (!anchor) return;
  noteReturnFocus = document.activeElement as HTMLElement | null;
  hideToolbar();
  document.querySelector(".gr-note-modal")?.remove();
  const modal = document.createElement("div");
  modal.className = "gr-note-modal";
  modal.dataset.goodreaderUi = "true";
  modal.setAttribute("role", "presentation");
  modal.innerHTML = `
    <div class="gr-note-card" role="dialog" aria-modal="true" aria-labelledby="grNoteTitle">
      <div class="gr-note-head">
        <div>
          <span class="gr-note-label">${annotation ? "编辑笔记" : "新建笔记"}</span>
          <h2 id="grNoteTitle">${annotation ? "修改你的想法" : "记录阅读想法"}</h2>
        </div>
        <button class="gr-note-close" type="button" data-cancel aria-label="关闭">${readerIcon("close")}</button>
      </div>
      <blockquote>${escapeHtml(anchor.quote)}</blockquote>
      <label for="grNoteText">笔记内容</label>
      <textarea id="grNoteText" maxlength="4000" placeholder="写下你的想法…">${escapeHtml(annotation?.note ?? "")}</textarea>
      <div class="gr-note-meta"><span id="grNoteCount">${(annotation?.note ?? "").length} / 4000</span><span>自动保存在本机</span></div>
      <div class="gr-note-actions">
        <button data-cancel>取消</button>
        <button class="primary" data-save>保存笔记</button>
      </div>
    </div>
  `;
  document.body.append(modal);
  const textarea = modal.querySelector<HTMLTextAreaElement>("textarea");
  textarea?.focus();
  textarea?.setSelectionRange(textarea.value.length, textarea.value.length);
  textarea?.addEventListener("input", () => {
    const count = modal.querySelector("#grNoteCount");
    if (count) count.textContent = `${textarea.value.length} / 4000`;
  });
  modal.querySelectorAll("[data-cancel]").forEach((button) => {
    button.addEventListener("click", closeNoteEditor);
  });
  modal.addEventListener("mousedown", (event) => {
    if (event.target === modal) closeNoteEditor();
  });
  modal.querySelector("[data-save]")?.addEventListener("click", () => {
    const note = textarea?.value.trim() ?? "";
    if (!note) {
      toast("笔记内容不能为空");
      return;
    }
    if (annotation) {
      void updateNote(annotation, note).then(closeNoteEditor);
      return;
    }
    currentSelection = anchor;
    void createAnnotation("note", undefined, note).then(closeNoteEditor);
  });
}

function closeNoteEditor(): void {
  document.querySelector(".gr-note-modal")?.remove();
  const target = noteReturnFocus;
  noteReturnFocus = null;
  if (target?.isConnected) target.focus();
}

async function updateNote(annotation: Annotation, note: string): Promise<void> {
  try {
    const updated = await api<Annotation>(
      `/api/annotations/${encodeURIComponent(annotation.id)}`,
      { method: "PUT", body: JSON.stringify({ note }) },
    );
    annotations = annotations.map((item) => (item.id === updated.id ? updated : item));
    renderAnnotations();
    toast("笔记已更新");
  } catch (error) {
    toast(error instanceof Error ? error.message : "更新失败");
  }
}

function confirmDeleteAnnotation(annotation: Annotation): void {
  noteReturnFocus = document.activeElement as HTMLElement | null;
  const modal = document.createElement("div");
  modal.className = "gr-note-modal gr-confirm-modal";
  modal.dataset.goodreaderUi = "true";
  modal.innerHTML = `
    <div class="gr-confirm-card" role="alertdialog" aria-modal="true" aria-labelledby="grConfirmTitle">
      <span class="gr-confirm-symbol">${readerIcon("warning")}</span>
      <h2 id="grConfirmTitle">删除这条${{ highlight: "高亮", note: "笔记", bookmark: "书签" }[annotation.kind]}？</h2>
      <blockquote>${escapeHtml(annotation.quote)}</blockquote>
      <p>删除后无法恢复，书籍正文不会受到影响。</p>
      <div class="gr-note-actions">
        <button data-cancel>取消</button>
        <button class="danger" data-confirm-delete>删除</button>
      </div>
    </div>
  `;
  document.body.append(modal);
  modal.querySelector("[data-cancel]")?.addEventListener("click", closeNoteEditor);
  modal.addEventListener("mousedown", (event) => {
    if (event.target === modal) closeNoteEditor();
  });
  modal.querySelector<HTMLButtonElement>("[data-confirm-delete]")?.addEventListener("click", () => {
    void deleteAnnotation(annotation).then(closeNoteEditor);
  });
  window.setTimeout(() => modal.querySelector<HTMLButtonElement>("[data-cancel]")?.focus());
}

async function deleteAnnotation(annotation: Annotation): Promise<void> {
  try {
    await api<void>(`/api/annotations/${encodeURIComponent(annotation.id)}`, {
      method: "DELETE",
      body: "{}",
    });
    annotations = annotations.filter((item) => item.id !== annotation.id);
    renderHighlights();
    renderAnnotations();
    toast("标注已删除");
  } catch (error) {
    toast(error instanceof Error ? error.message : "删除失败");
  }
}

function renderHighlights(): void {
  if (!content) return;
  clearInlineAnnotations();
  const registry = (CSS as unknown as { highlights?: HighlightRegistryLike }).highlights;
  const HighlightConstructor = (
    window as unknown as { Highlight?: new (...ranges: Range[]) => unknown }
  ).Highlight;
  if (!registry || !HighlightConstructor) {
    renderInlineAnnotations();
    return;
  }
  registry.clear();
  const groups: Record<string, Range[]> = {
    "gr-yellow": [],
    "gr-green": [],
    "gr-blue": [],
    "gr-pink": [],
    "gr-note": [],
    "gr-bookmark": [],
  };
  for (const annotation of annotations) {
    if (annotation.chapterId !== chapterId) continue;
    const range = rangeFromAnnotation(annotation);
    if (!range) continue;
    if (annotation.kind === "highlight") groups[`gr-${annotation.color ?? "yellow"}`].push(range);
    if (annotation.kind === "note") groups["gr-note"].push(range);
    if (annotation.kind === "bookmark") groups["gr-bookmark"].push(range);
  }
  Object.entries(groups).forEach(([name, ranges]) => {
    if (ranges.length) registry.set(name, new HighlightConstructor(...ranges));
  });
  document.documentElement.dataset.goodreaderAnnotationRenderer = "css";
}

function clearInlineAnnotations(): void {
  document.querySelectorAll<HTMLElement>(".gr-inline-annotation").forEach((element) => {
    element.replaceWith(...element.childNodes);
  });
  content?.normalize();
}

function renderInlineAnnotations(): void {
  const current = annotations
    .filter((annotation) => annotation.chapterId === chapterId)
    .sort((left, right) => left.startOffset - right.startOffset || left.endOffset - right.endOffset);
  let markers = 0;
  for (const annotation of current) {
    const block = document.querySelector<HTMLElement>(
      `[data-goodreader-block="${CSS.escape(annotation.blockId)}"]`,
    );
    if (block) markers += wrapTextRange(block, annotation);
  }
  document.documentElement.dataset.goodreaderAnnotationRenderer = `inline:${markers}`;
}

function wrapTextRange(block: HTMLElement, annotation: Annotation): number {
  let absoluteOffset = 0;
  let markers = 0;
  for (const node of textNodes(block)) {
    const nodeStart = absoluteOffset;
    const nodeEnd = nodeStart + node.length;
    absoluteOffset = nodeEnd;
    if (nodeEnd <= annotation.startOffset) continue;
    if (nodeStart >= annotation.endOffset) break;

    const start = Math.max(0, annotation.startOffset - nodeStart);
    const end = Math.min(node.length, annotation.endOffset - nodeStart);
    if (end <= start) continue;
    const range = document.createRange();
    range.setStart(node, start);
    range.setEnd(node, end);
    const marker = document.createElement("span");
    marker.className = [
      "gr-inline-annotation",
      `gr-inline-${annotation.kind}`,
      annotation.color ? `gr-inline-${annotation.color}` : "",
    ]
      .filter(Boolean)
      .join(" ");
    marker.dataset.annotationId = annotation.id;
    range.surroundContents(marker);
    markers += 1;
  }
  return markers;
}

function jumpToAnnotation(annotation: Annotation): void {
  if (annotation.chapterId !== chapterId) {
    const chapter = book.chapters.find((item) => item.id === annotation.chapterId);
    if (chapter) {
      window.location.href = `${chapter.url}?annotation=${encodeURIComponent(annotation.id)}`;
    }
    return;
  }
  const block = document.querySelector<HTMLElement>(
    `[data-goodreader-block="${CSS.escape(annotation.blockId)}"]`,
  );
  block?.scrollIntoView({ behavior: "smooth", block: "center" });
  toggleSidebar(false);
}

async function showParallelText(): Promise<void> {
  if (!currentSelection) return;
  try {
    if (parallelCache === undefined) {
      parallelCache = await api<ParallelText>(
        `/api/books/${encodeURIComponent(bookId)}/parallel/${encodeURIComponent(chapterId)}`,
      );
    }
    const text = parallelCache?.blocks[currentSelection.blockId];
    if (!text) {
      toast("当前正文块没有对照原文");
      return;
    }
    hideToolbar();
    removeParallelCard();
    const card = document.createElement("aside");
    card.className = "gr-parallel-card";
    card.dataset.goodreaderUi = "true";
    card.setAttribute("aria-label", "对照原文");
    card.tabIndex = -1;
    card.innerHTML = `
      <div><button type="button" aria-label="关闭对照原文">${readerIcon("close")}</button></div>
      <p>${escapeHtml(text)}</p>
    `;
    document.body.append(card);
    card.querySelector("button")?.addEventListener("click", removeParallelCard);
    card.addEventListener("focusout", () => {
      window.setTimeout(() => {
        if (!card.contains(document.activeElement)) removeParallelCard();
      });
    });
    card.focus({ preventScroll: true });
  } catch (error) {
    toast(error instanceof Error ? error.message : "无法读取对照原文");
  }
}

function bindProgress(): void {
  if (!content || !chapterId) return;
  const schedule = () => {
    if (progressTimer !== null) window.clearTimeout(progressTimer);
    progressTimer = window.setTimeout(() => void saveCurrentProgress(), 350);
  };
  window.addEventListener("scroll", schedule, { passive: true });
  window.addEventListener("resize", schedule);
  window.addEventListener("pagehide", () => void saveCurrentProgress(true));
  window.setTimeout(() => {
    if (new URLSearchParams(window.location.search).has("resume")) resumeProgress();
    const annotationId = new URLSearchParams(window.location.search).get("annotation");
    if (annotationId) {
      const annotation = annotations.find((item) => item.id === annotationId);
      if (annotation) jumpToAnnotation(annotation);
    }
    const blockId = new URLSearchParams(window.location.search).get("block");
    if (blockId) {
      document
        .querySelector<HTMLElement>(`[data-goodreader-block="${CSS.escape(blockId)}"]`)
        ?.scrollIntoView({ behavior: "smooth", block: "center" });
    }
    schedule();
  }, 180);
}

function currentChapterProgress(): number {
  if (!content) return 0;
  const rect = content.getBoundingClientRect();
  const top = rect.top + window.scrollY;
  const scrollable = Math.max(1, content.scrollHeight - window.innerHeight);
  return clamp((window.scrollY - top) / scrollable);
}

function nearestBlockId(): string | null {
  if (!content) return null;
  const blocks = [...content.querySelectorAll<HTMLElement>("[data-goodreader-block]")];
  const target = 86;
  let best: HTMLElement | undefined;
  let distance = Number.POSITIVE_INFINITY;
  for (const block of blocks) {
    const candidate = Math.abs(block.getBoundingClientRect().top - target);
    if (candidate < distance) {
      distance = candidate;
      best = block;
    }
  }
  return best?.dataset.goodreaderBlock ?? null;
}

async function saveCurrentProgress(keepalive = false): Promise<void> {
  const chapterIndex = book.chapters.findIndex((chapter) => chapter.id === chapterId);
  if (chapterIndex < 0) return;
  const chapterProgress = currentChapterProgress();
  const overallProgress = clamp((chapterIndex + chapterProgress) / book.chapters.length);
  updateProgressLabel(overallProgress);
  const body = JSON.stringify({
    chapterId,
    blockId: nearestBlockId(),
    chapterProgress,
    overallProgress,
  });
  try {
    await api<Progress>(`/api/books/${encodeURIComponent(bookId)}/progress`, {
      method: "PUT",
      body,
      keepalive,
    });
  } catch {
    if (!keepalive) toast("阅读进度暂未保存");
  }
}

function resumeProgress(): void {
  if (!book.progress || book.progress.chapterId !== chapterId) return;
  if (book.progress.blockId) {
    document
      .querySelector<HTMLElement>(
        `[data-goodreader-block="${CSS.escape(book.progress.blockId)}"]`,
      )
      ?.scrollIntoView({ block: "start" });
  } else if (content) {
    const rect = content.getBoundingClientRect();
    const top = rect.top + window.scrollY;
    const scrollable = Math.max(1, content.scrollHeight - window.innerHeight);
    window.scrollTo({ top: top + scrollable * book.progress.chapterProgress });
  }
  updateProgressLabel(book.progress.overallProgress);
}

function updateProgressLabel(value: number): void {
  const label = document.querySelector("#grProgressLabel");
  if (label) label.textContent = value >= 0.995 ? "已读完" : `${Math.round(value * 100)}%`;
  const progress = document.querySelector<HTMLElement>("#grReadingProgress");
  const percent = Math.round(clamp(value) * 100);
  progress?.setAttribute("aria-valuenow", String(percent));
  const indicator = progress?.querySelector<HTMLElement>("i");
  if (indicator) indicator.style.width = `${percent}%`;
}

function clamp(value: number): number {
  return Math.max(0, Math.min(1, value));
}

function bindExternalLinks(): void {
  document.addEventListener("click", (event) => {
    const anchor = (event.target as Element | null)?.closest<HTMLAnchorElement>("a[href]");
    if (!anchor) return;
    const url = new URL(anchor.href, window.location.href);
    if (url.origin === window.location.origin) return;
    if (!["http:", "https:"].includes(url.protocol)) {
      event.preventDefault();
      return;
    }
    event.preventDefault();
    void api<void>("/api/open-external", {
      method: "POST",
      body: JSON.stringify({ url: url.href }),
    });
  });
}

function toast(message: string): void {
  let element = document.querySelector<HTMLDivElement>(".gr-toast");
  if (!element) {
    element = document.createElement("div");
    element.className = "gr-toast";
    element.dataset.goodreaderUi = "true";
    document.body.append(element);
  }
  element.textContent = message;
  element.classList.add("visible");
  window.setTimeout(() => element?.classList.remove("visible"), 2200);
}

async function loadPreference(
  key: "highlight-color" | "annotation-filter" | "reader-theme" | "ai-send-key" | "topbar-pinned",
): Promise<string | null> {
  try {
    const setting = await api<{ value: string | null }>(`/api/settings/${key}`);
    return setting.value;
  } catch {
    return null;
  }
}

async function savePreference(
  key: "highlight-color" | "annotation-filter" | "reader-theme" | "ai-send-key" | "topbar-pinned",
  value: string,
): Promise<void> {
  try {
    await api<void>(`/api/settings/${key}`, {
      method: "PUT",
      body: JSON.stringify({ value }),
    });
  } catch {
    toast("设置暂未保存");
  }
}

function applyBookLanguage(): void {
  const language = book.language?.trim().toLowerCase() ?? "";
  if (language === "zh" || language.startsWith("zh-")) {
    document.documentElement.dataset.goodreaderLanguage = "zh";
    document.documentElement.lang = "zh-CN";
    return;
  }
  delete document.documentElement.dataset.goodreaderLanguage;
}

async function boot(): Promise<void> {
  if (!bookId) return;
  try {
    [book, annotations] = await Promise.all([
      api<Book>(`/api/books/${encodeURIComponent(bookId)}`),
      api<Annotation[]>(`/api/books/${encodeURIComponent(bookId)}/annotations`),
    ]);
    const [savedColor, savedFilter, savedTheme, savedAiSendKey, savedTopbarPinned] = await Promise.all([
      loadPreference("highlight-color"),
      loadPreference("annotation-filter"),
      loadPreference("reader-theme"),
      loadPreference("ai-send-key"),
      loadPreference("topbar-pinned"),
    ]);
    if (savedColor && savedColor in colorLabels) highlightColor = savedColor as HighlightColor;
    if (["all", "highlight", "note", "bookmark"].includes(savedFilter ?? "")) {
      annotationFilter = savedFilter as typeof annotationFilter;
    }
    if (["system", "light", "dark"].includes(savedTheme ?? "")) {
      readerTheme = savedTheme as ReaderTheme;
    }
    if (savedAiSendKey === "enter" || savedAiSendKey === "mod-enter") {
      aiSendKey = savedAiSendKey;
    }
    topbarPinned = savedTopbarPinned === "true";
    aiViewState = restoreAiViewState();
    restoreAiWorkspaceCache();
    applyBookLanguage();
    applyReaderTheme();
    injectChrome();
    bindExternalLinks();
    if (content && chapterId) {
      bindSelection();
      renderHighlights();
      bindProgress();
    }
  } catch (error) {
    toast(error instanceof Error ? error.message : "GoodReader 阅读器加载失败");
  }
}

systemTheme.addEventListener("change", () => {
  if (readerTheme === "system") applyReaderTheme();
});
applyReaderTheme();
void boot();
