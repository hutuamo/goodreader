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
  originalTitle: string | null;
  author: string;
  coverUrl: string;
  entryUrl: string;
  chapters: Chapter[];
  progress: Progress | null;
};

type ImportIssue = {
  path: string;
  title: string;
  detail: string;
};

type Bootstrap = {
  books: Book[];
  issues: ImportIssue[];
  libraryPath: string;
};

type ImportedBook = {
  id: string;
  title: string;
  chapterCount: number;
  warnings: string[];
};

type ImportBookResponse = {
  cancelled: boolean;
  imported: ImportedBook | null;
  bootstrap: Bootstrap;
};

type ReplaceCoverResponse = {
  changed: boolean;
  bootstrap: Bootstrap;
};

type ImportSourceKind = "html" | "pdf" | "url";
type PdfImportMode = "auto" | "text-layer" | "ocr";

type ImportChapterCandidate = {
  id: string;
  title: string;
  source: string;
  selected: boolean;
};

type ImportPreflight = {
  token: string;
  kind: ImportSourceKind;
  sourceName: string;
  title: string;
  originalTitle: string;
  author: string;
  language: string;
  languageConfidence: string;
  pageCount: number | null;
  chapterCandidates: ImportChapterCandidate[];
  imageCount: number;
  characterCount: number;
  requiresOcrPages: number[];
  uncertainPages: number[];
  pdfMode: PdfImportMode | null;
  pdfType: "digital" | "scanned" | "mixed" | null;
  dynamicRendering: boolean;
  warnings: string[];
};

type ImportQualityReport = {
  errors: string[];
  warnings: string[];
  chapterCount: number;
  blockCount: number;
  imageCount: number;
  translatedBlockCount: number;
  originalBlockCount: number;
};

type ImportTask = {
  id: string;
  status: "queued" | "running" | "paused" | "failed" | "cancelled" | "completed";
  stage: string;
  progress: number;
  title: string;
  usesAgent: boolean;
  queueOrder: number;
  detail: string;
  error: string | null;
  imported: ImportedBook | null;
  quality: ImportQualityReport | null;
  createdAt: number;
  updatedAt: number;
};

type ImportTaskEvent = {
  id: string;
  seq: number;
  kind: "stage" | "script" | "agent" | "error" | string;
  title: string;
  detail: string;
  createdAt: number;
  scope?: string;
  state?: string;
  progress?: { completed: number; total: number; unit: string };
  timing?: { startedAt: number; elapsedMs: number; etaMs?: number };
  runtime?: { id: string; model?: string; sessionId?: string; pid?: number };
  metrics?: {
    batch: number;
    batches: number;
    blocks: number;
    chars: number;
    completedBlocks: number;
    totalBlocks: number;
    completedChars: number;
    totalChars: number;
    attempt: number;
  };
};

type Backup = {
  name: string;
  createdAt: number;
  size: number;
};

type AgentRuntime = {
  id: string;
  name: string;
  executable: string | null;
  available: boolean;
  version: string | null;
  detail: string | null;
  builtIn: boolean;
  capabilities: {
    streaming: boolean;
    nativeResume: boolean;
    structuredOutput: boolean;
    permissionMapping: boolean;
    toolUse: boolean;
  };
};

type ShelfFilter = "all" | "reading" | "unread" | "finished";

const appRoot = document.querySelector<HTMLDivElement>("#app");
if (!appRoot) throw new Error("缺少应用挂载节点");
const root: HTMLDivElement = appRoot;

let data: Bootstrap = { books: [], issues: [], libraryPath: "" };
let search = "";
let shelfFilter: ShelfFilter = "all";
let modalReturnFocus: HTMLElement | null = null;
let globalShortcutsBound = false;
let currentImportTask: ImportTask | null = null;
let currentImportPollTimer: number | null = null;
const expandedImportTaskIds = new Set<string>();
const importTaskEventCache = new Map<string, ImportTaskEvent[]>();
const importTaskEventRequests = new Set<string>();

const filterLabels: Record<ShelfFilter, string> = {
  all: "全部书籍",
  reading: "正在阅读",
  unread: "未开始",
  finished: "已读完",
};

function escapeHtml(value: string): string {
  return value.replace(
    /[&<>"']/g,
    (character) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#039;" })[
        character
      ] ?? character,
  );
}

function icon(name: string): string {
  const paths: Record<string, string> = {
    book: '<path d="M4.5 5.5A3.5 3.5 0 0 1 8 2h3v17H8a3.5 3.5 0 0 0-3.5 3.5v-17Z"/><path d="M19.5 5.5A3.5 3.5 0 0 0 16 2h-3v17h3a3.5 3.5 0 0 1 3.5 3.5v-17Z"/><path d="M15 14.5h2.5"/>',
    grid: '<rect x="3" y="3" width="7" height="7" rx="2"/><rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><rect x="14" y="14" width="7" height="7" rx="2"/>',
    reading: '<path d="M6 3h12v18l-6-4-6 4V3Z"/><path d="M9 8h6M9 12h4"/>',
    unread: '<path d="M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3V4Z"/><path d="M8 4v16"/>',
    finished: '<circle cx="12" cy="12" r="9"/><path d="m8 12 2.5 2.5L16.5 8.5"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-4-4"/>',
    folder: '<path d="M3 7h7l2-3h9v15a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/><path d="M3 9h18"/>',
    import: '<path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M4 17v3h16v-3"/>',
    refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 5v6h-6"/>',
    settings: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1-2.8 2.8-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.6v.2h-4V21a1.7 1.7 0 0 0-1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1L4.2 17l.1-.1a1.7 1.7 0 0 0 .3-1.9A1.7 1.7 0 0 0 3 14H2.8v-4H3a1.7 1.7 0 0 0 1.6-1 1.7 1.7 0 0 0-.3-1.9L4.2 7 7 4.2l.1.1A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-1.6v-.2h4V3a1.7 1.7 0 0 0 1 1.6 1.7 1.7 0 0 0 1.9-.3l.1-.1L19.8 7l-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0 1.6 1h.2v4H21a1.7 1.7 0 0 0-1.6 1Z"/>',
    warning: '<path d="M12 3 2.8 20h18.4L12 3Z"/><path d="M12 9v5M12 17.5v.1"/>',
    more: '<circle cx="5" cy="12" r="1.2" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.2" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="1.2" fill="currentColor" stroke="none"/>',
    arrow: '<path d="m9 18 6-6-6-6"/>',
    close: '<path d="m6 6 12 12M18 6 6 18"/>',
    backup: '<path d="M4 8a8 8 0 1 1 2.3 8.7"/><path d="M4 3v5h5"/><path d="M12 7v5l3 2"/>',
    check: '<path d="m5 12 4 4L19 6"/>',
    trash: '<path d="M4 7h16"/><path d="M9 3h6l1 4H8l1-4Z"/><path d="m6 7 1 14h10l1-14"/><path d="M10 11v6M14 11v6"/>',
    sparkles: '<path d="m12 3 1.4 4.1L17.5 8.5l-4.1 1.4L12 14l-1.4-4.1-4.1-1.4 4.1-1.4L12 3Z"/><path d="m19 14 .8 2.2L22 17l-2.2.8L19 20l-.8-2.2L16 17l2.2-.8L19 14ZM5 15l.8 2.2L8 18l-2.2.8L5 21l-.8-2.2L2 18l2.2-.8L5 15Z"/>',
    terminal: '<path d="m5 7 4 4-4 4M11 17h7"/><rect x="2.5" y="3.5" width="19" height="17" rx="3"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    queue: '<path d="M5 6h14M5 12h14M5 18h14"/><circle cx="3" cy="6" r=".7" fill="currentColor" stroke="none"/><circle cx="3" cy="12" r=".7" fill="currentColor" stroke="none"/><circle cx="3" cy="18" r=".7" fill="currentColor" stroke="none"/>',
    link: '<path d="M10 13a5 5 0 0 0 7.1.1l2-2a5 5 0 0 0-7.1-7.1l-1.1 1.1"/><path d="M14 11a5 5 0 0 0-7.1-.1l-2 2A5 5 0 0 0 12 20l1.1-1.1"/>',
    file: '<path d="M6 2h8l4 4v16H6V2Z"/><path d="M14 2v5h5"/><path d="M9 13h6M9 17h6"/>',
    image: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="2"/><path d="m4 18 5-5 3 3 3-4 5 6"/>',
    pause: '<path d="M8 5v14M16 5v14"/>',
    play: '<path d="m8 5 11 7-11 7V5Z"/>',
    chevron: '<path d="m6 9 6 6 6-6"/>',
  };
  return `<svg class="ui-icon" viewBox="0 0 24 24" aria-hidden="true">${paths[name] ?? paths.book}</svg>`;
}

function brandMark(): string {
  return `
    <span class="brand-mark" aria-hidden="true">
      <svg viewBox="0 0 32 32">
        <path d="M6 8.5c3.5 0 6.4 1.2 10 4.2v13c-3.6-3-6.5-4.2-10-4.2v-13Z"/>
        <path d="M26 8.5c-3.5 0-6.4 1.2-10 4.2v13c3.6-3 6.5-4.2 10-4.2v-13Z"/>
        <path class="brand-highlight" d="M19 18.2c1.5-.8 2.8-1.3 4.3-1.5"/>
      </svg>
    </span>
  `;
}

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

function formatProgress(book: Book): string {
  if (!book.progress) return "未开始";
  if (book.progress.overallProgress >= 0.995) return "已读完";
  const chapter = book.chapters.find((item) => item.id === book.progress?.chapterId);
  const progress = Math.max(1, Math.round(book.progress.overallProgress * 100));
  return `${progress}% · ${chapter?.title ?? "继续阅读"}`;
}

function sortedBooks(books: Book[]): Book[] {
  return [...books].sort((left, right) => {
    if (left.progress && right.progress) return right.progress.updatedAt - left.progress.updatedAt;
    if (left.progress) return -1;
    if (right.progress) return 1;
    return left.title.localeCompare(right.title, "zh-CN");
  });
}

function matchesFilter(book: Book, filter: ShelfFilter): boolean {
  if (filter === "unread") return !book.progress;
  if (filter === "finished") return (book.progress?.overallProgress ?? 0) >= 0.995;
  if (filter === "reading") {
    const progress = book.progress?.overallProgress ?? 0;
    return progress > 0 && progress < 0.995;
  }
  return true;
}

function filteredBooks(): Book[] {
  const normalized = search.trim().toLocaleLowerCase("zh-CN");
  return sortedBooks(data.books).filter((book) => {
    if (!matchesFilter(book, shelfFilter)) return false;
    if (!normalized) return true;
    return `${book.title} ${book.originalTitle ?? ""} ${book.author}`
      .toLocaleLowerCase("zh-CN")
      .includes(normalized);
  });
}

function filterCount(filter: ShelfFilter): number {
  return data.books.filter((book) => matchesFilter(book, filter)).length;
}

function render(): void {
  root.innerHTML = `
    <a class="skip-link" href="#libraryContent">跳到书籍内容</a>
    <div class="app-shell">
      <aside class="library-sidebar" aria-label="书架分类">
        <a class="brand" href="/" aria-label="GoodReader 书架">
          ${brandMark()}
          <span>GoodReader</span>
        </a>
        <nav class="shelf-navigation">
          ${(Object.keys(filterLabels) as ShelfFilter[])
            .map(
              (filter) => `
                <button
                  class="${filter === shelfFilter ? "active" : ""}"
                  data-filter="${filter}"
                  aria-current="${filter === shelfFilter ? "page" : "false"}"
                >
                  ${icon({ all: "grid", reading: "reading", unread: "unread", finished: "finished" }[filter])}
                  <span>${filterLabels[filter]}</span>
                  <strong data-filter-count="${filter}">${filterCount(filter)}</strong>
                </button>
              `,
            )
            .join("")}
        </nav>
        <div class="sidebar-spacer"></div>
        ${currentImportProgressEntry()}
        ${
          data.issues.length
            ? `<button class="sidebar-issue" id="showIssues">
                ${icon("warning")}
                <span><strong>导入问题</strong><small>${data.issues.length} 项需要处理</small></span>
                ${icon("arrow")}
              </button>`
            : ""
        }
        <div class="sidebar-footer">
          <span>仅存储在这台 Mac</span>
          <button class="sidebar-settings" id="settings" aria-label="备份与设置">
            ${icon("settings")}
          </button>
        </div>
      </aside>

      <section class="workspace">
        <header class="workspace-toolbar">
          <label class="search-box">
            <span class="visually-hidden">搜索书名或作者</span>
            ${icon("search")}
            <input
              id="searchInput"
              type="search"
              placeholder="搜索书名或作者"
              value="${escapeHtml(search)}"
              autocomplete="off"
            />
            <kbd>⌘ K</kbd>
          </label>
          <div class="toolbar-actions" aria-label="书架操作">
            <button class="secondary-icon-button" id="importTasks" aria-label="书籍生成任务" title="生成任务">
              ${icon("queue")}
            </button>
            <button class="secondary-icon-button" id="rescan" aria-label="重新扫描书库" title="重新扫描">
              ${icon("refresh")}
            </button>
            <button class="secondary-icon-button" id="openLibrary" aria-label="在 Finder 中打开书库" title="打开书库">
              ${icon("folder")}
            </button>
            <button class="primary-button" id="importBook">
              ${icon(currentImportTask ? "queue" : "import")}
              <span>${currentImportTask ? `导入中 ${currentImportTask.progress}%` : "导入书籍"}</span>
            </button>
          </div>
        </header>
        <main class="library" id="libraryContent" tabindex="-1">
          ${libraryContent()}
        </main>
      </section>
    </div>
    <div id="modalRoot"></div>
    <div class="toast" id="toast" role="status" aria-live="polite"></div>
  `;
  bindEvents();
}

function isUnfinishedImport(task: ImportTask): boolean {
  return !["completed", "cancelled"].includes(task.status);
}

function currentImportProgressEntry(): string {
  const task = currentImportTask;
  return `
    <button class="sidebar-import-progress ${task ? "active" : ""}" id="currentImportProgress" type="button">
      <span class="sidebar-import-icon">${icon("queue")}</span>
      <span class="sidebar-import-copy">
        <strong id="currentImportProgressTitle">${task ? escapeHtml(task.title) : "导入进度"}</strong>
        <small id="currentImportProgressDetail">${task ? `${escapeHtml(importStageLabel(task.stage))} · ${escapeHtml(task.detail)}` : "暂无正在导入"}</small>
        <span class="sidebar-import-track" aria-hidden="true"><i id="currentImportProgressBar" style="width:${task?.progress ?? 0}%"></i></span>
      </span>
      <strong class="sidebar-import-value" id="currentImportProgressValue">${task ? `${task.progress}%` : ""}</strong>
    </button>
  `;
}

function libraryContent(): string {
  const books = filteredBooks();
  const normalized = search.trim();
  const current = shelfFilter === "all" && !normalized
    ? sortedBooks(data.books).find((book) => {
        const progress = book.progress?.overallProgress ?? 0;
        return progress > 0 && progress < 0.995;
      })
    : undefined;

  return `
    <section class="library-heading">
      <div>
        <p class="eyebrow">本地书库</p>
        <h1>${filterLabels[shelfFilter]}</h1>
        <p>${normalized ? `“${escapeHtml(normalized)}”的搜索结果` : shelfDescription()}</p>
      </div>
      <span class="library-count">${books.length} 本</span>
    </section>
    ${current ? continueReading(current) : ""}
    ${
      books.length
        ? `<section aria-labelledby="booksHeading">
            <div class="section-heading">
              <h2 id="booksHeading">${current ? "所有书籍" : filterLabels[shelfFilter]}</h2>
              <span>按最近阅读排序</span>
            </div>
            <div class="book-grid">
              ${books.map((book, index) => bookCard(book, index)).join("")}
            </div>
          </section>`
        : emptyState(Boolean(normalized))
    }
  `;
}

function shelfDescription(): string {
  if (shelfFilter === "reading") return "接着上次停下的位置继续。";
  if (shelfFilter === "unread") return "还没有打开过的书籍。";
  if (shelfFilter === "finished") return "已经完成阅读的书籍。";
  return data.books.length ? `你的 ${data.books.length} 本书都在这里。` : "从 PDF、HTML 目录或在线链接生成本地书籍。";
}

function continueReading(book: Book): string {
  const progress = Math.max(1, Math.round((book.progress?.overallProgress ?? 0) * 100));
  const chapter = book.chapters.find((item) => item.id === book.progress?.chapterId);
  return `
    <section class="continue-card" aria-labelledby="continueHeading">
      <img src="${escapeHtml(book.coverUrl)}" alt="" />
      <div class="continue-copy">
        <p>继续阅读</p>
        <h2 id="continueHeading">${escapeHtml(book.title)}</h2>
        <span>${escapeHtml(chapter?.title ?? "继续阅读")} · ${progress}%</span>
        <div class="continue-progress" role="progressbar" aria-label="阅读进度" aria-valuenow="${progress}" aria-valuemin="0" aria-valuemax="100">
          <i style="width:${progress}%"></i>
        </div>
      </div>
      <button class="continue-button" data-continue="${escapeHtml(book.id)}">
        <span>继续阅读</span>
        ${icon("arrow")}
      </button>
    </section>
  `;
}

function bookCard(book: Book, index: number): string {
  const measuredProgress = Math.round(Math.max(0, Math.min(1, book.progress?.overallProgress ?? 0)) * 100);
  const progress = book.progress ? Math.max(1, measuredProgress) : 0;
  return `
    <article class="book-card" data-book-id="${escapeHtml(book.id)}" style="--card-index:${Math.min(index, 8)}">
      <div class="cover-wrap">
        <button class="cover-button" data-action="read" aria-label="打开《${escapeHtml(book.title)}》">
          <span class="cover-frame">
            <img src="${escapeHtml(book.coverUrl)}" alt="${escapeHtml(book.title)}封面" loading="lazy" width="320" height="448" />
          </span>
        </button>
        <button class="more-button" data-action="menu" aria-label="《${escapeHtml(book.title)}》更多操作" aria-haspopup="menu">
          ${icon("more")}
        </button>
      </div>
      <div class="book-meta">
        <button class="book-title" data-action="read">${escapeHtml(book.title)}</button>
        <p class="book-author">${escapeHtml(book.author)}</p>
        <div class="book-status">
          <span>${escapeHtml(formatProgress(book))}</span>
          ${
            progress
              ? `<div class="book-progress-track" role="progressbar" aria-label="《${escapeHtml(book.title)}》阅读进度" aria-valuenow="${progress}" aria-valuemin="0" aria-valuemax="100"><i style="width:${progress}%"></i></div>`
              : ""
          }
        </div>
      </div>
    </article>
  `;
}

function emptyState(filtered: boolean): string {
  if (filtered) {
    return `
      <section class="empty-state">
        <span class="empty-icon">${icon("search")}</span>
        <h2>没有找到匹配的书</h2>
        <p>检查书名或作者，也可以清除搜索后浏览全部书籍。</p>
        <button class="secondary-button" id="clearSearch">清除搜索</button>
      </section>
    `;
  }
  if (data.books.length) {
    return `
      <section class="empty-state">
        <span class="empty-icon">${icon("book")}</span>
        <h2>这里暂时没有书</h2>
        <p>切换到“全部书籍”查看完整书架。</p>
        <button class="secondary-button" id="showAllBooks">查看全部书籍</button>
      </section>
    `;
  }
  return `
    <section class="empty-state">
      <span class="empty-icon">${icon("folder")}</span>
      <h2>从第一本书开始</h2>
      <p>选择 PDF、本地 HTML 目录或在线链接，GoodReader 会生成统一格式，来源不会被修改。</p>
      <div class="empty-actions">
        <button class="primary-button" id="emptyImportBook">${icon(currentImportTask ? "queue" : "import")}<span>${currentImportTask ? `查看导入进度 ${currentImportTask.progress}%` : "导入书籍"}</span></button>
        <button class="secondary-button" id="emptyOpenLibrary">${icon("folder")}<span>打开书库</span></button>
      </div>
    </section>
  `;
}

function renderLibraryOnly(): void {
  const library = document.querySelector<HTMLElement>("#libraryContent");
  if (!library) return;
  library.innerHTML = libraryContent();
  bindLibraryEvents();
}

function bindEvents(): void {
  const input = document.querySelector<HTMLInputElement>("#searchInput");
  input?.addEventListener("input", () => {
    search = input.value;
    renderLibraryOnly();
  });
  input?.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && input.value) {
      search = "";
      input.value = "";
      renderLibraryOnly();
    }
  });

  if (!globalShortcutsBound) {
    document.addEventListener("keydown", handleGlobalShortcut);
    globalShortcutsBound = true;
  }

  document.querySelectorAll<HTMLElement>("[data-filter]").forEach((button) => {
    button.addEventListener("click", () => {
      shelfFilter = button.dataset.filter as ShelfFilter;
      render();
      document.querySelector<HTMLElement>('[data-filter][aria-current="page"]')?.focus();
    });
  });

  const openLibrary = () =>
    run(async () => {
      await api<void>("/api/open-library", { method: "POST", body: "{}" });
    });
  document.querySelector("#openLibrary")?.addEventListener("click", openLibrary);
  document.querySelector("#importBook")?.addEventListener("click", (event) =>
    openImportEntry(event.currentTarget as HTMLButtonElement),
  );

  document.querySelector("#rescan")?.addEventListener("click", (event) =>
    runWithButton(event.currentTarget as HTMLButtonElement, async () => {
      data = await api<Bootstrap>("/api/rescan", { method: "POST", body: "{}" });
      render();
      toast(`扫描完成，共 ${data.books.length} 本书`);
    }),
  );

  document.querySelector("#settings")?.addEventListener("click", () => void showSettings());
  document.querySelector("#importTasks")?.addEventListener("click", () => void showImportTasks());
  document.querySelector("#currentImportProgress")?.addEventListener("click", (event) =>
    openImportEntry(event.currentTarget as HTMLButtonElement),
  );
  document.querySelector("#showIssues")?.addEventListener("click", showIssues);
  bindLibraryEvents();
}

function bindLibraryEvents(): void {
  document.querySelector("#clearSearch")?.addEventListener("click", () => {
    search = "";
    const input = document.querySelector<HTMLInputElement>("#searchInput");
    if (input) input.value = "";
    renderLibraryOnly();
    input?.focus();
  });
  document.querySelector("#showAllBooks")?.addEventListener("click", () => {
    shelfFilter = "all";
    render();
  });
  document.querySelector("#emptyOpenLibrary")?.addEventListener("click", () =>
    run(async () => {
      await api<void>("/api/open-library", { method: "POST", body: "{}" });
    }),
  );
  document.querySelector("#emptyImportBook")?.addEventListener("click", (event) => {
    openImportEntry(event.currentTarget as HTMLButtonElement);
  });
  document.querySelectorAll<HTMLElement>("[data-continue]").forEach((button) => {
    const book = data.books.find((item) => item.id === button.dataset.continue);
    if (book) button.addEventListener("click", () => openBook(book));
  });
  document.querySelectorAll<HTMLElement>(".book-card").forEach((card) => {
    const book = data.books.find((item) => item.id === card.dataset.bookId);
    if (!book) return;
    card.querySelectorAll<HTMLElement>('[data-action="read"]').forEach((button) => {
      button.addEventListener("click", () => openBook(book));
    });
    card
      .querySelector<HTMLElement>('[data-action="menu"]')
      ?.addEventListener("click", (event) =>
        showBookMenu(book, event.currentTarget as HTMLButtonElement),
      );
  });
}

function openImportEntry(button: HTMLButtonElement): void {
  if (currentImportTask) {
    modalReturnFocus = button;
    showImportTaskProgress(currentImportTask);
    return;
  }
  void importBook(button);
}

async function importBook(button: HTMLButtonElement): Promise<void> {
  if (currentImportTask) {
    modalReturnFocus = button;
    showImportTaskProgress(currentImportTask);
    return;
  }
  modalReturnFocus = button;
  showModal(`
    <div class="modal-header">
      <div>
        <p class="modal-kicker">生成 GoodReader 书籍</p>
        <h2 id="modalTitle">选择书籍来源</h2>
      </div>
      <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
    </div>
    <div class="import-source-grid">
      <button class="import-source-card" data-import-source="pdf">
        <span>${icon("file")}</span><strong>PDF 文件</strong><small>每页由 Agent 恢复阅读顺序、书籍排版和完整图片；扫描页会在预检中提示 OCR</small>
      </button>
      <button class="import-source-card" data-import-source="html">
        <span>${icon("folder")}</span><strong>本地 HTML 目录</strong><small>复制来源快照，移除原有脚本并接入统一阅读器</small>
      </button>
      <button class="import-source-card" data-import-source="url">
        <span>${icon("link")}</span><strong>在线链接</strong><small>发现同源章节，下载资源并生成可离线阅读的静态书籍</small>
      </button>
    </div>
    <form class="url-import-form" id="pdfImportForm" hidden>
      <div class="settings-section-heading"><h3>选择 PDF 文本来源</h3><span>可在自动判断不准确时覆盖</span></div>
      <label class="option-row">
        <input type="radio" name="pdfMode" value="auto" checked />
        <span><strong>自动识别</strong><small>逐页检查文本层，发现扫描正文时暂停并报告页码</small></span>
      </label>
      <label class="option-row">
        <input type="radio" name="pdfMode" value="text-layer" />
        <span><strong>使用 PDF 文本层</strong><small>适用于数字 PDF 或已经完成 OCR、具有可用文本层的文件</small></span>
      </label>
      <label class="option-row">
        <input type="radio" name="pdfMode" value="ocr" />
        <span><strong>扫描 PDF，需要 OCR</strong><small>当前版本尚未配置本地 OCR，预检后会暂停生成</small></span>
      </label>
      <div class="modal-actions"><button class="secondary-button" type="button" id="cancelPdfImport">返回</button><button class="primary-button" type="submit">选择 PDF</button></div>
    </form>
    <form class="url-import-form" id="urlImportForm" hidden>
      <label><span>公开链接</span><input id="importUrl" type="url" required placeholder="https://example.com/book/" autocomplete="url" /></label>
      <div class="modal-actions"><button class="secondary-button" type="button" id="cancelUrlImport">返回</button><button class="primary-button" type="submit">分析链接</button></div>
    </form>
    <p class="import-privacy-note">所有转换在这台 Mac 上完成。原始来源不会被修改，书籍通过质量校验后才进入书架。</p>
  `);
  document.querySelectorAll<HTMLButtonElement>("[data-import-source]").forEach((sourceButton) => {
    sourceButton.addEventListener("click", () => {
      const kind = sourceButton.dataset.importSource as ImportSourceKind;
      if (kind === "url") {
        document.querySelector<HTMLElement>(".import-source-grid")!.hidden = true;
        document.querySelector<HTMLFormElement>("#urlImportForm")!.hidden = false;
        document.querySelector<HTMLInputElement>("#importUrl")?.focus();
        return;
      }
      if (kind === "pdf") {
        document.querySelector<HTMLElement>(".import-source-grid")!.hidden = true;
        document.querySelector<HTMLFormElement>("#pdfImportForm")!.hidden = false;
        document.querySelector<HTMLInputElement>('input[name="pdfMode"]')?.focus();
        return;
      }
      void runWithButton(sourceButton, async () => {
        const preflight = await api<ImportPreflight>("/api/import/preflight", {
          method: "POST",
          body: JSON.stringify({ kind }),
        });
        await showImportConfiguration(preflight);
      });
    });
  });
  document.querySelector("#cancelUrlImport")?.addEventListener("click", () => {
    document.querySelector<HTMLElement>(".import-source-grid")!.hidden = false;
    document.querySelector<HTMLFormElement>("#urlImportForm")!.hidden = true;
  });
  document.querySelector("#cancelPdfImport")?.addEventListener("click", () => {
    document.querySelector<HTMLElement>(".import-source-grid")!.hidden = false;
    document.querySelector<HTMLFormElement>("#pdfImportForm")!.hidden = true;
  });
  document.querySelector<HTMLFormElement>("#pdfImportForm")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const submit = form.querySelector<HTMLButtonElement>('button[type="submit"]');
    const pdfMode = form.querySelector<HTMLInputElement>('input[name="pdfMode"]:checked')?.value as PdfImportMode | undefined;
    if (!submit || !pdfMode) return;
    void runWithButton(submit, async () => {
      const preflight = await api<ImportPreflight>("/api/import/preflight", {
        method: "POST",
        body: JSON.stringify({ kind: "pdf", pdfMode }),
      });
      await showImportConfiguration(preflight);
    });
  });
  document.querySelector<HTMLFormElement>("#urlImportForm")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const submit = form.querySelector<HTMLButtonElement>('button[type="submit"]');
    const url = document.querySelector<HTMLInputElement>("#importUrl")?.value.trim() ?? "";
    if (!submit || !url) return;
    void runWithButton(submit, async () => {
      const preflight = await api<ImportPreflight>("/api/import/preflight", {
        method: "POST",
        body: JSON.stringify({ kind: "url", url }),
      });
      await showImportConfiguration(preflight);
    });
  });
}

async function showImportConfiguration(preflight: ImportPreflight): Promise<void> {
  const needsLayoutAgent = preflight.kind === "pdf";
  const runtimes = preflight.language === "zh-CN" && !needsLayoutAgent
    ? []
    : await api<AgentRuntime[]>("/api/agent/runtimes");
  const availableRuntimes = runtimes.filter((runtime) => runtime.available);
  const isChinese = preflight.language === "zh-CN";
  const languageLabel = isChinese
    ? "中文为主"
    : preflight.language === "non-zh"
      ? "非中文为主"
      : "混合或无法确定";
  const ocrBlocked = preflight.requiresOcrPages.length > 0;
  const pdfTypeLabel = preflight.pdfType
    ? { digital: "数字 PDF", scanned: "扫描 PDF", mixed: "混合 PDF" }[preflight.pdfType]
    : null;
  showModal(`
    <div class="modal-header">
      <div>
        <p class="modal-kicker">预检完成 · ${escapeHtml(sourceKindLabel(preflight.kind))}</p>
        <h2 id="modalTitle">确认书籍结构与生成方式</h2>
      </div>
      <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
    </div>
    <form id="importConfiguration" class="import-configuration">
      <section class="preflight-summary">
        <div><span>来源</span><strong>${escapeHtml(preflight.sourceName)}</strong></div>
        <div><span>语言判断</span><strong>${languageLabel}</strong></div>
        ${pdfTypeLabel ? `<div><span>PDF 类型</span><strong>${pdfTypeLabel}</strong></div>` : ""}
        <div><span>规模</span><strong>${preflight.pageCount ? `${preflight.pageCount} 页 · ` : ""}${preflight.characterCount.toLocaleString("zh-CN")} 字符</strong></div>
        <div><span>图片</span><strong>${preflight.imageCount} 项</strong></div>
      </section>
      ${ocrBlocked ? `<div class="import-blocker">${icon("warning")}<div><strong>需要本地 OCR，当前不能开始生成</strong><p>正文扫描页：${formatPageList(preflight.requiresOcrPages)}。任务停留在预检阶段，不会生成残缺书籍。</p></div></div>` : ""}
      ${needsLayoutAgent && !availableRuntimes.length ? `<div class="import-blocker">${icon("warning")}<div><strong>需要可用 Agent，当前不能开始生成</strong><p>PDF 的每一页都需要由 Agent 恢复阅读顺序、语义块和完整图片区域。请先在设置中配置 Agent。</p></div></div>` : ""}
      ${preflight.warnings.map((warning) => `<div class="import-warning">${icon("warning")}<span>${escapeHtml(warning)}</span></div>`).join("")}
      <div class="import-fields">
        <label><span>书名</span><input id="importTitle" required maxlength="240" value="${escapeHtml(preflight.title)}" /></label>
        <label><span>作者</span><input id="importAuthor" required maxlength="240" value="${escapeHtml(preflight.author)}" /></label>
      </div>
      <section class="chapter-editor">
        <div class="settings-section-heading"><h3>章节清单</h3><span>${preflight.chapterCandidates.length} 个候选</span></div>
        <div class="chapter-candidate-list">
          ${preflight.chapterCandidates.map((chapter, index) => `
            <label class="chapter-candidate">
              <input type="checkbox" data-chapter-selected="${index}" ${chapter.selected ? "checked" : ""} />
              <input type="text" data-chapter-title="${index}" maxlength="240" value="${escapeHtml(chapter.title)}" aria-label="章节标题" />
              <small title="${escapeHtml(chapter.source)}">${escapeHtml(chapter.source)}</small>
            </label>
          `).join("")}
        </div>
      </section>
      <section class="translation-options ${isChinese ? "disabled" : ""}">
        <label class="option-row">
          <input id="translateBook" type="checkbox" ${isChinese || needsLayoutAgent || !availableRuntimes.length ? "disabled" : ""} />
          <span><strong>翻译为简体中文</strong><small>${isChinese ? "来源已经是中文，不需要翻译" : needsLayoutAgent ? "PDF 来源暂不支持翻译，仅由 Agent 排版正文" : availableRuntimes.length ? "由本机 Agent 完成，耗时长于仅转换" : "没有可用 Agent，请先在设置中配置"}</small></span>
        </label>
        <label class="option-row">
          <input id="preserveOriginal" type="checkbox" disabled />
          <span><strong>支持显示原文</strong><small>保存正文块级原文并建立对齐，需要更长生成时间</small></span>
        </label>
        <label class="agent-select-row" id="importAgentRow" ${needsLayoutAgent ? "" : "hidden"}><span>${needsLayoutAgent ? "PDF 排版 Agent" : "执行 Agent"}</span><select id="importRuntime">${availableRuntimes.map((runtime) => `<option value="${escapeHtml(runtime.id)}">${escapeHtml(runtime.name)} · ${escapeHtml(runtime.version ?? "可用")}</option>`).join("")}</select></label>
      </section>
      <div class="import-workload"><strong>工作量提示</strong><p id="workloadText">${needsLayoutAgent ? "逐页调用 Agent 恢复阅读顺序、书籍排版和完整图片区域；每页完成后保存检查点。" : "仅执行确定性转换与安全校验。"}</p></div>
      <div class="modal-actions">
        <button class="secondary-button" type="button" data-close>取消</button>
        <button class="primary-button" id="startImportTask" type="submit" ${ocrBlocked || (needsLayoutAgent && !availableRuntimes.length) ? "disabled" : ""}>开始生成</button>
      </div>
    </form>
  `, "wide import-wizard");

  const translate = document.querySelector<HTMLInputElement>("#translateBook");
  const preserve = document.querySelector<HTMLInputElement>("#preserveOriginal");
  const agentRow = document.querySelector<HTMLElement>("#importAgentRow");
  const workload = document.querySelector<HTMLElement>("#workloadText");
  const updateTranslationOptions = () => {
    const enabled = Boolean(translate?.checked);
    if (preserve) {
      preserve.disabled = !enabled;
      if (!enabled) preserve.checked = false;
    }
    if (agentRow) agentRow.hidden = !needsLayoutAgent && !enabled;
    if (workload) {
      workload.textContent = !enabled
        ? needsLayoutAgent
          ? "逐页调用 Agent 恢复阅读顺序、书籍排版和完整图片区域；每页完成后保存检查点。"
          : "仅执行确定性转换与安全校验。"
        : preserve?.checked
          ? `${needsLayoutAgent ? "逐页 Agent 排版后，" : ""}翻译并建立正文块级原文对齐，属于耗时最长的生成方式。`
          : `${needsLayoutAgent ? "逐页 Agent 排版后，" : ""}翻译为简体中文，耗时取决于正文块数量和 Agent 速度。`;
    }
  };
  translate?.addEventListener("change", updateTranslationOptions);
  preserve?.addEventListener("change", updateTranslationOptions);
  document.querySelector<HTMLFormElement>("#importConfiguration")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const button = document.querySelector<HTMLButtonElement>("#startImportTask");
    if (!button) return;
    const chapters = preflight.chapterCandidates.map((chapter, index) => ({
      ...chapter,
      selected: document.querySelector<HTMLInputElement>(`[data-chapter-selected="${index}"]`)?.checked ?? false,
      title: document.querySelector<HTMLInputElement>(`[data-chapter-title="${index}"]`)?.value.trim() || chapter.title,
    }));
    void runWithButton(button, async () => {
      const task = await api<ImportTask>("/api/import/tasks", {
        method: "POST",
        body: JSON.stringify({
          token: preflight.token,
          title: document.querySelector<HTMLInputElement>("#importTitle")?.value.trim() ?? preflight.title,
          author: document.querySelector<HTMLInputElement>("#importAuthor")?.value.trim() ?? preflight.author,
          chapters,
          translate: translate?.checked ?? false,
          preserveOriginal: preserve?.checked ?? false,
          runtimeId: needsLayoutAgent || translate?.checked ? document.querySelector<HTMLSelectElement>("#importRuntime")?.value : null,
        }),
      });
      showImportTaskProgress(task);
    });
  });
}

function sourceKindLabel(kind: ImportSourceKind): string {
  return { html: "本地 HTML", pdf: "PDF", url: "在线链接" }[kind];
}

function formatPageList(pages: number[]): string {
  if (pages.length <= 18) return pages.join("、");
  return `${pages.slice(0, 18).join("、")} 等 ${pages.length} 页`;
}

function showImportTaskProgress(initial: ImportTask): void {
  showModal(`
    <div class="import-task-progress-view" data-import-task-id="${escapeHtml(initial.id)}">
      <div class="modal-header">
        <div><p class="modal-kicker">后台书籍生成</p><h2 id="modalTitle"></h2></div>
        <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
      </div>
      <div class="task-progress-card" id="importTaskProgressCard">
        <div class="task-progress-heading"><strong id="importTaskStage"></strong><span id="importTaskPercent"></span></div>
        <div class="task-progress-track"><i id="importTaskProgressBar"></i></div>
        <p id="importTaskDetail"></p>
        <div id="importTaskError"></div>
        <div id="importTaskQuality"></div>
        <section class="import-task-details-section">
          <button class="import-task-details-toggle" id="importTaskDetailsToggle" type="button" aria-expanded="false" aria-controls="importTaskDetailsPanel">
            <span>${icon("terminal")}<strong>生成进度详情</strong><small id="importTaskDetailsCount">尚无记录</small></span>
            <span class="import-task-details-chevron">${icon("chevron")}</span>
          </button>
          <div class="import-task-details-panel" id="importTaskDetailsPanel" hidden>
            <div class="import-task-details-summary" id="importTaskDetailsSummary"></div>
            <div class="import-task-event-list" id="importTaskEventList" aria-live="polite"></div>
          </div>
        </section>
      </div>
      <div class="modal-actions" id="importTaskProgressActions"></div>
    </div>
  `, "wide import-wizard");
  bindImportTaskDetails(initial.id);
  applyImportTaskUpdate(initial);
}

function updateImportTaskProgress(task: ImportTask): void {
  const view = document.querySelector<HTMLElement>(`[data-import-task-id="${CSS.escape(task.id)}"]`);
  if (!view) return;
  const title = view.querySelector<HTMLElement>("#modalTitle");
  const stage = view.querySelector<HTMLElement>("#importTaskStage");
  const percent = view.querySelector<HTMLElement>("#importTaskPercent");
  const progressBar = view.querySelector<HTMLElement>("#importTaskProgressBar");
  const detail = view.querySelector<HTMLElement>("#importTaskDetail");
  const error = view.querySelector<HTMLElement>("#importTaskError");
  const quality = view.querySelector<HTMLElement>("#importTaskQuality");
  const actions = view.querySelector<HTMLElement>("#importTaskProgressActions");
  if (!title || !stage || !percent || !progressBar || !detail || !error || !quality || !actions) return;

  title.textContent = task.title;
  stage.textContent = importStageLabel(task.stage);
  percent.textContent = `${task.progress}%`;
  progressBar.style.width = `${task.progress}%`;
  detail.textContent = task.detail;
  error.innerHTML = task.error ? `<div class="import-blocker">${icon("warning")}<div><strong>任务已暂停</strong><p>${escapeHtml(task.error)}</p></div></div>` : "";
  quality.innerHTML = task.quality ? qualityReportHtml(task.quality) : "";

  if (view.dataset.taskStatus !== task.status) {
    view.dataset.taskStatus = task.status;
    actions.innerHTML = `
      <button class="secondary-button" data-close>${task.status === "completed" ? "完成" : "在后台运行"}</button>
      ${task.status === "running" || task.status === "queued" ? `<button class="secondary-button" id="pauseCurrentImport">${icon("pause")}暂停</button>` : ""}
      ${task.status === "paused" || task.status === "failed" ? `<button class="primary-button" id="resumeCurrentImport">${icon("play")}继续</button>` : ""}
      ${task.status === "failed" && task.usesAgent ? `<button class="secondary-button" id="switchImportAgent">${icon("terminal")}更换 Agent</button>` : ""}
      ${task.status !== "completed" && task.status !== "cancelled" ? `<button class="danger-button" id="cancelCurrentImport">取消任务</button>` : ""}
      ${task.status === "completed" && task.imported ? `<button class="primary-button" id="openGeneratedBook">开始阅读</button>` : ""}
    `;
    actions.querySelectorAll("[data-close]").forEach((button) => {
      button.addEventListener("click", closeModal);
    });
    bindImportTaskActions(task);
  }

  updateImportTaskDetailsState(task.id);
}

function bindImportTaskDetails(taskId: string): void {
  const toggle = document.querySelector<HTMLButtonElement>("#importTaskDetailsToggle");
  if (!toggle) return;
  toggle.addEventListener("click", () => {
    if (expandedImportTaskIds.has(taskId)) expandedImportTaskIds.delete(taskId);
    else expandedImportTaskIds.add(taskId);
    updateImportTaskDetailsState(taskId);
    if (expandedImportTaskIds.has(taskId)) void refreshImportTaskEvents(taskId);
  });
}

function updateImportTaskDetailsState(taskId: string): void {
  const view = document.querySelector<HTMLElement>(`[data-import-task-id="${CSS.escape(taskId)}"]`);
  if (!view) return;
  const toggle = view.querySelector<HTMLButtonElement>("#importTaskDetailsToggle");
  const panel = view.querySelector<HTMLElement>("#importTaskDetailsPanel");
  const count = view.querySelector<HTMLElement>("#importTaskDetailsCount");
  if (!toggle || !panel || !count) return;
  const expanded = expandedImportTaskIds.has(taskId);
  toggle.setAttribute("aria-expanded", String(expanded));
  panel.hidden = !expanded;
  view.classList.toggle("details-expanded", expanded);
  const events = importTaskEventCache.get(taskId);
  count.textContent = events ? `${events.length} 条记录` : "点击查看";
  if (expanded) renderImportTaskEvents(taskId);
}

async function refreshImportTaskEvents(taskId: string): Promise<void> {
  if (importTaskEventRequests.has(taskId)) return;
  importTaskEventRequests.add(taskId);
  try {
    const cached = importTaskEventCache.get(taskId) ?? [];
    const afterSeq = cached.reduce((highest, event) => Math.max(highest, event.seq || 0), 0);
    const events = await api<ImportTaskEvent[]>(`/api/import/tasks/${encodeURIComponent(taskId)}/events?afterSeq=${afterSeq}`);
    const merged = new Map(cached.map((event) => [event.id, event]));
    merged.delete("live-agent-output");
    for (const event of events) merged.set(event.id, event);
    importTaskEventCache.set(taskId, [...merged.values()].sort((left, right) => {
      if (!left.seq || !right.seq) return left.createdAt - right.createdAt;
      return left.seq - right.seq;
    }));
    updateImportTaskDetailsState(taskId);
  } catch (error) {
    const list = document.querySelector<HTMLElement>("#importTaskEventList");
    if (list && expandedImportTaskIds.has(taskId)) {
      list.innerHTML = `<div class="empty-task-events">${icon("warning")}<span>${escapeHtml(error instanceof Error ? error.message : "无法读取生成详情")}</span></div>`;
    }
  } finally {
    importTaskEventRequests.delete(taskId);
  }
}

function renderImportTaskEvents(taskId: string): void {
  const view = document.querySelector<HTMLElement>(`[data-import-task-id="${CSS.escape(taskId)}"]`);
  const list = view?.querySelector<HTMLElement>("#importTaskEventList");
  const summary = view?.querySelector<HTMLElement>("#importTaskDetailsSummary");
  if (!list) return;
  const events = importTaskEventCache.get(taskId);
  if (!events) {
    list.innerHTML = `<div class="empty-task-events loading-events"><span>正在读取生成详情…</span></div>`;
    return;
  }
  if (!events.length) {
    if (summary) summary.innerHTML = "";
    list.innerHTML = `<div class="empty-task-events"><span>尚无生成详情</span></div>`;
    return;
  }

  const latestMeasured = [...events].reverse().find((event) => event.metrics || event.runtime || event.timing);
  if (summary) summary.innerHTML = importTaskEventSummaryHtml(latestMeasured);

  const nearBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 36;
  const previousScrollTop = list.scrollTop;
  list.innerHTML = events.map((event) => {
    const style = importEventStyle(event.kind);
    const metadata = importTaskEventMetadata(event);
    return `
      <article class="import-task-event ${style.className}">
        <span class="import-task-event-icon">${icon(style.iconName)}</span>
        <div>
          <header><strong>${escapeHtml(event.title)}</strong><time datetime="${new Date(event.createdAt).toISOString()}">${formatImportEventTime(event.createdAt)}</time></header>
          ${metadata ? `<p class="import-task-event-metadata">${metadata}</p>` : ""}
          <pre>${escapeHtml(event.detail || "无文本输出")}</pre>
        </div>
      </article>
    `;
  }).join("");
  list.scrollTop = nearBottom ? list.scrollHeight : previousScrollTop;
}

function importTaskEventSummaryHtml(event: ImportTaskEvent | undefined): string {
  if (!event) return `<p class="import-task-detail-placeholder">等待第一条可度量的生成记录…</p>`;
  const metrics = event.metrics;
  const timing = event.timing;
  const runtime = event.runtime;
  const cards = [
    metrics ? `<div><small>当前批次</small><strong>${metrics.batch}/${metrics.batches}</strong></div>` : "",
    metrics ? `<div><small>正文块</small><strong>${metrics.completedBlocks.toLocaleString("zh-CN")}/${metrics.totalBlocks.toLocaleString("zh-CN")}</strong></div>` : "",
    metrics ? `<div><small>字符</small><strong>${formatCompactCount(metrics.completedChars)}/${formatCompactCount(metrics.totalChars)}</strong></div>` : "",
    timing ? `<div><small>本批耗时</small><strong>${formatDuration(timing.elapsedMs)}</strong></div>` : "",
    timing?.etaMs ? `<div><small>预计剩余</small><strong>${formatDuration(timing.etaMs)}</strong></div>` : "",
    runtime ? `<div><small>Agent</small><strong>${escapeHtml(runtime.id)}${runtime.model ? ` · ${escapeHtml(runtime.model)}` : ""}${runtime.pid ? ` · PID ${runtime.pid}` : ""}</strong></div>` : "",
  ].filter(Boolean).join("");
  return `<div class="import-task-detail-cards">${cards}</div>`;
}

function importTaskEventMetadata(event: ImportTaskEvent): string {
  const items: string[] = [];
  if (event.metrics) {
    items.push(`批次 ${event.metrics.batch}/${event.metrics.batches}`);
    items.push(`${event.metrics.blocks} 块`);
    items.push(`${event.metrics.chars.toLocaleString("zh-CN")} 字符`);
    if (event.metrics.attempt > 1) items.push(`${event.metrics.attempt} 次执行`);
  }
  if (event.timing?.elapsedMs) items.push(`耗时 ${formatDuration(event.timing.elapsedMs)}`);
  if (event.runtime?.sessionId) items.push(`会话 ${event.runtime.sessionId.slice(0, 12)}`);
  if (event.runtime?.model) items.push(`模型 ${event.runtime.model}`);
  if (event.state === "running") items.push("运行中");
  return items.map(escapeHtml).join(" · ");
}

function formatDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.round(milliseconds / 1000));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  if (hours) return `${hours}小时${minutes}分`;
  if (minutes) return `${minutes}分${rest}秒`;
  return `${rest}秒`;
}

function formatCompactCount(value: number): string {
  if (value >= 10000) return `${(value / 10000).toFixed(1)}万`;
  return value.toLocaleString("zh-CN");
}

function importEventStyle(kind: string): { className: string; iconName: string } {
  if (kind === "agent") return { className: "agent", iconName: "sparkles" };
  if (kind === "script") return { className: "script", iconName: "terminal" };
  if (kind === "error") return { className: "error", iconName: "warning" };
  return { className: "stage", iconName: "check" };
}

function formatImportEventTime(timestamp: number): string {
  return new Intl.DateTimeFormat("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(new Date(timestamp));
}

function applyImportTaskUpdate(task: ImportTask): void {
  updateImportTaskProgress(task);
  const wasCurrent = currentImportTask?.id === task.id;
  if (isUnfinishedImport(task)) currentImportTask = task;
  else if (wasCurrent) currentImportTask = null;
  updateCurrentImportChrome();
  scheduleCurrentImportPoll();
  if (expandedImportTaskIds.has(task.id)) void refreshImportTaskEvents(task.id);
  if (task.status === "completed" && wasCurrent) void refreshLibraryAfterImport();
}

function updateCurrentImportChrome(): void {
  const task = currentImportTask;
  const entry = document.querySelector<HTMLButtonElement>("#currentImportProgress");
  entry?.classList.toggle("active", Boolean(task));
  entry?.setAttribute("aria-label", task ? `查看《${task.title}》导入进度，${task.progress}%` : "查看导入记录");

  const title = document.querySelector<HTMLElement>("#currentImportProgressTitle");
  const detail = document.querySelector<HTMLElement>("#currentImportProgressDetail");
  const value = document.querySelector<HTMLElement>("#currentImportProgressValue");
  const bar = document.querySelector<HTMLElement>("#currentImportProgressBar");
  if (title) title.textContent = task?.title ?? "导入进度";
  if (detail) detail.textContent = task ? `${importStageLabel(task.stage)} · ${task.detail}` : "暂无正在导入";
  if (value) value.textContent = task ? `${task.progress}%` : "";
  if (bar) bar.style.width = `${task?.progress ?? 0}%`;

  const importButton = document.querySelector<HTMLButtonElement>("#importBook");
  if (importButton) {
    importButton.innerHTML = `${icon(task ? "queue" : "import")}<span>${task ? `导入中 ${task.progress}%` : "导入书籍"}</span>`;
    importButton.setAttribute("aria-label", task ? "查看当前导入进度" : "导入书籍");
  }
  const emptyImportButton = document.querySelector<HTMLButtonElement>("#emptyImportBook");
  if (emptyImportButton) {
    emptyImportButton.innerHTML = `${icon(task ? "queue" : "import")}<span>${task ? `查看导入进度 ${task.progress}%` : "导入书籍"}</span>`;
  }
}

function scheduleCurrentImportPoll(): void {
  if (currentImportPollTimer !== null) window.clearTimeout(currentImportPollTimer);
  currentImportPollTimer = null;
  if (!currentImportTask || !["queued", "running"].includes(currentImportTask.status)) return;
  currentImportPollTimer = window.setTimeout(() => {
    currentImportPollTimer = null;
    void refreshCurrentImportTask();
  }, 850);
}

async function refreshCurrentImportTask(): Promise<void> {
  const id = currentImportTask?.id;
  if (!id) return;
  try {
    const task = await api<ImportTask>(`/api/import/tasks/${encodeURIComponent(id)}`);
    applyImportTaskUpdate(task);
  } catch (error) {
    console.warn("刷新导入进度失败", error);
    scheduleCurrentImportPoll();
  }
}

async function refreshLibraryAfterImport(): Promise<void> {
  await run(async () => {
    data = await api<Bootstrap>("/api/bootstrap");
    search = "";
    shelfFilter = "all";
    renderLibraryOnly();
    document.querySelectorAll<HTMLElement>("[data-filter-count]").forEach((count) => {
      count.textContent = String(filterCount(count.dataset.filterCount as ShelfFilter));
    });
    updateCurrentImportChrome();
  });
}

function bindImportTaskActions(task: ImportTask): void {
  document.querySelector<HTMLButtonElement>("#pauseCurrentImport")?.addEventListener("click", (event) => {
    void runWithButton(event.currentTarget as HTMLButtonElement, async () => {
      applyImportTaskUpdate(await api<ImportTask>(`/api/import/tasks/${encodeURIComponent(task.id)}/pause`, { method: "POST", body: "{}" }));
    });
  });
  document.querySelector<HTMLButtonElement>("#resumeCurrentImport")?.addEventListener("click", (event) => {
    void runWithButton(event.currentTarget as HTMLButtonElement, async () => {
      const resumed = await api<ImportTask>(`/api/import/tasks/${encodeURIComponent(task.id)}/resume`, { method: "POST", body: "{}" });
      applyImportTaskUpdate(resumed);
    });
  });
  document.querySelector<HTMLButtonElement>("#cancelCurrentImport")?.addEventListener("click", (event) => {
    void runWithButton(event.currentTarget as HTMLButtonElement, async () => {
      applyImportTaskUpdate(await api<ImportTask>(`/api/import/tasks/${encodeURIComponent(task.id)}/cancel`, { method: "POST", body: "{}" }));
    });
  });
  document.querySelector<HTMLButtonElement>("#switchImportAgent")?.addEventListener("click", () => {
    void showImportAgentSwitch(task);
  });
  document.querySelector("#openGeneratedBook")?.addEventListener("click", () => {
    const book = data.books.find((item) => item.id === task.imported?.id);
    if (book) openBook(book);
  });
}

async function showImportAgentSwitch(task: ImportTask): Promise<void> {
  await run(async () => {
    const runtimes = (await api<AgentRuntime[]>("/api/agent/runtimes")).filter((runtime) => runtime.available);
    if (!runtimes.length) throw new Error("当前没有可用 Agent");
    showModal(`
      <div class="modal-header"><div><p class="modal-kicker">保持同一生成任务与检查点</p><h2 id="modalTitle">选择接续任务的 Agent</h2></div><button class="modal-close" data-close aria-label="关闭">${icon("close")}</button></div>
      <div class="agent-runtime-list">${runtimes.map((runtime) => `<button data-resume-runtime="${escapeHtml(runtime.id)}"><span class="runtime-icon">${icon("terminal")}</span><span><strong>${escapeHtml(runtime.name)}</strong><small>${escapeHtml(runtime.version ?? "可用")}</small></span>${icon("arrow")}</button>`).join("")}</div>
    `);
    document.querySelectorAll<HTMLButtonElement>("[data-resume-runtime]").forEach((button) => {
      button.addEventListener("click", () => {
        void runWithButton(button, async () => {
          const resumed = await api<ImportTask>(`/api/import/tasks/${encodeURIComponent(task.id)}/resume`, {
            method: "POST",
            body: JSON.stringify({ runtimeId: button.dataset.resumeRuntime }),
          });
          showImportTaskProgress(resumed);
        });
      });
    });
  });
}

function importStageLabel(stage: string): string {
  return {
    queued: "等待生成",
    snapshot: "校验来源快照",
    converting: "转换静态 HTML",
    contract: "建立书籍契约",
    translating: "Agent 翻译",
    validating: "质量与安全校验",
    publishing: "写入书架",
    paused: "已暂停",
    failed: "需要处理",
    completed: "生成完成",
    cancelled: "已取消",
  }[stage] ?? stage;
}

function qualityReportHtml(report: ImportQualityReport): string {
  return `<div class="quality-report"><h3>质量报告</h3><div><span>${report.chapterCount} 章</span><span>${report.blockCount} 个正文块</span><span>${report.imageCount} 张图片</span>${report.translatedBlockCount ? `<span>${report.translatedBlockCount} 个译文块</span>` : ""}${report.originalBlockCount ? `<span>${report.originalBlockCount} 个原文块</span>` : ""}</div>${report.warnings.length ? `<ul>${report.warnings.map((warning) => `<li>${escapeHtml(warning)}</li>`).join("")}</ul>` : ""}</div>`;
}

async function showImportTasks(): Promise<void> {
  await run(async () => {
    const tasks = await api<ImportTask[]>("/api/import/tasks");
    const orderedTasks = [...tasks].sort((left, right) => {
      const leftActive = ["running", "queued"].includes(left.status);
      const rightActive = ["running", "queued"].includes(right.status);
      if (leftActive !== rightActive) return leftActive ? -1 : 1;
      if (left.status === "queued" && right.status === "queued") return left.queueOrder - right.queueOrder;
      return right.updatedAt - left.updatedAt;
    });
    showModal(`
      <div class="modal-header"><div><p class="modal-kicker">本地持久任务</p><h2 id="modalTitle">书籍生成任务</h2></div><button class="modal-close" data-close aria-label="关闭">${icon("close")}</button></div>
      <div class="import-task-list">
        ${orderedTasks.length ? orderedTasks.map((task, index) => `<div class="import-task-row"><button data-import-task="${escapeHtml(task.id)}"><span class="task-status ${task.status}">${task.progress}%</span><span><strong>${escapeHtml(task.title)}</strong><small>${escapeHtml(importStageLabel(task.stage))} · ${escapeHtml(task.detail)}</small></span>${icon("arrow")}</button>${task.status === "queued" ? `<div class="queue-actions"><button data-move-task="${escapeHtml(task.id)}" data-direction="-1" aria-label="在队列中上移" title="上移" ${index === 0 ? "disabled" : ""}>↑</button><button data-move-task="${escapeHtml(task.id)}" data-direction="1" aria-label="在队列中下移" title="下移">↓</button></div>` : ""}</div>`).join("") : `<div class="empty-task-list">${icon("queue")}<strong>暂无生成任务</strong><span>从“导入书籍”创建第一个任务。</span></div>`}
      </div>
    `);
    document.querySelectorAll<HTMLButtonElement>("[data-import-task]").forEach((taskButton) => {
      taskButton.addEventListener("click", () => {
        const task = tasks.find((item) => item.id === taskButton.dataset.importTask);
        if (task) showImportTaskProgress(task);
      });
    });
    document.querySelectorAll<HTMLButtonElement>("[data-move-task]").forEach((button) => {
      button.addEventListener("click", () => {
        void runWithButton(button, async () => {
          await api<ImportTask[]>(`/api/import/tasks/${encodeURIComponent(button.dataset.moveTask ?? "")}/move`, {
            method: "POST",
            body: JSON.stringify({ direction: Number(button.dataset.direction) }),
          });
          await showImportTasks();
        });
      });
    });
  });
}

function showImportComplete(imported: ImportedBook): void {
  const book = data.books.find((item) => item.id === imported.id);
  modalReturnFocus = document.querySelector<HTMLButtonElement>("#importBook");
  showModal(
    `
      <div class="modal-symbol success">${icon("check")}</div>
      <p class="modal-kicker">转换完成</p>
      <h2 id="modalTitle">《${escapeHtml(imported.title)}》已导入</h2>
      <p>已生成 ${imported.chapterCount} 个章节并写入 GoodReader 书库。原始 HTML 目录没有被修改。</p>
      <div class="import-report">
        ${
          imported.warnings.length
            ? `<h3>转换说明</h3>
               <ul>${imported.warnings.map((warning) => `<li>${escapeHtml(warning)}</li>`).join("")}</ul>`
            : `<span>${icon("check")} HTML 内容已通过安全检查，没有需要处理的项目。</span>`
        }
      </div>
      <div class="modal-actions">
        <button class="secondary-button" data-close>完成</button>
        ${book ? `<button class="primary-button" id="openImportedBook" data-autofocus>开始阅读</button>` : ""}
      </div>
    `,
    "compact",
  );
  if (book) {
    document.querySelector("#openImportedBook")?.addEventListener("click", () => openBook(book));
  }
}

function handleGlobalShortcut(event: KeyboardEvent): void {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    document.querySelector<HTMLInputElement>("#searchInput")?.focus();
  }
}

function openBook(book: Book): void {
  if (!book.progress) {
    window.location.href = book.entryUrl;
    return;
  }
  const chapter = book.chapters.find((item) => item.id === book.progress?.chapterId);
  window.location.href = `${chapter?.url ?? book.entryUrl}?resume=1`;
}

function showBookMenu(book: Book, anchor: HTMLButtonElement): void {
  closeBookMenu();
  const menu = document.createElement("div");
  menu.className = "book-menu";
  menu.setAttribute("role", "menu");
  menu.innerHTML = `
    <button role="menuitem" data-menu="home">${icon("book")}<span>打开书籍首页</span></button>
    <button role="menuitem" data-menu="cover">${icon("image")}<span>替换封面</span></button>
    <div class="menu-separator"></div>
    <button role="menuitem" data-menu="delete" class="danger-item">${icon("trash")}<span>删除书籍副本</span></button>
    <button role="menuitem" data-menu="forget" class="danger-item">${icon("warning")}<span>永久忘记全部数据</span></button>
  `;
  const rect = anchor.getBoundingClientRect();
  menu.style.top = `${rect.bottom + 8}px`;
  menu.style.left = `${Math.max(12, rect.right - 236)}px`;
  document.body.append(menu);
  anchor.setAttribute("aria-expanded", "true");

  const close = () => {
    menu.remove();
    anchor.removeAttribute("aria-expanded");
  };
  menu.querySelector<HTMLButtonElement>('[data-menu="home"]')?.addEventListener("click", () => {
    close();
    window.location.href = book.entryUrl;
  });
  menu.querySelector<HTMLButtonElement>('[data-menu="cover"]')?.addEventListener("click", () => {
    close();
    void replaceBookCover(book);
  });
  menu.querySelector<HTMLButtonElement>('[data-menu="delete"]')?.addEventListener("click", () => {
    close();
    void confirmDeleteBook(book);
  });
  menu.querySelector<HTMLButtonElement>('[data-menu="forget"]')?.addEventListener("click", () => {
    close();
    void confirmForget(book);
  });
  menu.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      close();
      anchor.focus();
    }
  });
  window.setTimeout(() => {
    document.addEventListener(
      "pointerdown",
      (event) => {
        if (!menu.contains(event.target as Node) && event.target !== anchor) close();
      },
      { once: true },
    );
    menu.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus();
  });
}

async function replaceBookCover(book: Book): Promise<void> {
  await run(async () => {
    const response = await api<ReplaceCoverResponse>(
      `/api/books/${encodeURIComponent(book.id)}/cover`, { method: "POST", body: "{}" },
    );
    if (!response.changed) return;
    data = response.bootstrap;
    render();
    toast("书籍封面已替换");
  });
}

function closeBookMenu(): void {
  document.querySelector(".book-menu")?.remove();
  document.querySelector('[aria-expanded="true"]')?.removeAttribute("aria-expanded");
}

async function confirmDeleteBook(book: Book): Promise<void> {
  await run(async () => {
    const { count } = await api<{ count: number }>(
      `/api/books/${encodeURIComponent(book.id)}/annotation-count`,
    );
    showModal(
      `
        <div class="modal-symbol danger">${icon("trash")}</div>
        <p class="modal-kicker">可从废纸篓恢复</p>
        <h2 id="modalTitle">删除《${escapeHtml(book.title)}》的书库副本？</h2>
        <p>GoodReader 书库中的副本将移到 macOS 废纸篓。原始导入目录不受影响，阅读进度和 ${count} 条高亮、笔记或书签会继续保留。</p>
        <div class="modal-actions">
          <button class="secondary-button" data-close>取消</button>
          <button class="danger-button" id="confirmDeleteBook" data-autofocus>移到废纸篓</button>
        </div>
      `,
      "compact",
    );
    document.querySelector("#confirmDeleteBook")?.addEventListener("click", (event) =>
      runWithButton(event.currentTarget as HTMLButtonElement, async () => {
        data = await api<Bootstrap>(
          `/api/books/${encodeURIComponent(book.id)}/package`,
          {
            method: "DELETE",
            body: "{}",
          },
        );
        closeModal();
        render();
        toast("书籍副本已移到废纸篓，阅读数据已保留");
      }),
    );
  });
}

async function confirmForget(book: Book): Promise<void> {
  await run(async () => {
    const { count } = await api<{ count: number }>(
      `/api/books/${encodeURIComponent(book.id)}/annotation-count`,
    );
    showModal(
      `
        <div class="modal-symbol danger">${icon("warning")}</div>
        <p class="modal-kicker">不可恢复</p>
        <h2 id="modalTitle">永久忘记《${escapeHtml(book.title)}》？</h2>
        <p>将删除阅读进度、${count} 条高亮、笔记或书签，以及这本书的全部 AI 历史和产物。磁盘中的书籍包不会被删除。</p>
        <div class="modal-actions">
          <button class="secondary-button" data-close>取消</button>
          <button class="danger-button" id="confirmForget" data-autofocus>永久删除</button>
        </div>
      `,
      "compact",
    );
    document.querySelector("#confirmForget")?.addEventListener("click", (event) =>
      runWithButton(event.currentTarget as HTMLButtonElement, async () => {
        await api<void>(`/api/books/${encodeURIComponent(book.id)}/forget`, {
          method: "DELETE",
          body: "{}",
        });
        closeModal();
        data = await api<Bootstrap>("/api/bootstrap");
        render();
        toast("这本书的阅读与 AI 数据已永久删除");
      }),
    );
  });
}

function showIssues(): void {
  showModal(`
    <div class="modal-header">
      <div>
        <p class="modal-kicker">导入诊断</p>
        <h2 id="modalTitle">导入问题</h2>
      </div>
      <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
    </div>
    <div class="issue-list">
      ${data.issues
        .map(
          (issue) => `
            <article>
              <span class="issue-symbol">${icon("warning")}</span>
              <div>
                <h3>${escapeHtml(issue.title)}</h3>
                <p>${escapeHtml(issue.detail)}</p>
                <code>${escapeHtml(issue.path)}</code>
              </div>
            </article>
          `,
        )
        .join("")}
    </div>
  `);
}

async function showSettings(): Promise<void> {
  const trigger = document.activeElement as HTMLElement | null;
  await run(async () => {
    const [backups, runtimes] = await Promise.all([
      api<Backup[]>("/api/backups"),
      api<AgentRuntime[]>("/api/agent/runtimes"),
    ]);
    modalReturnFocus = trigger;
    showModal(`
      <div class="modal-header">
        <div>
          <p class="modal-kicker">本地数据</p>
          <h2 id="modalTitle">备份与恢复</h2>
        </div>
        <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
      </div>
      <div class="settings-summary">
        <span class="settings-icon">${icon("folder")}</span>
        <div><span>书库位置</span><strong title="${escapeHtml(data.libraryPath)}">${escapeHtml(data.libraryPath)}</strong></div>
        <button class="primary-button" id="backupNow">${icon("backup")}<span>立即备份</span></button>
      </div>
      <div class="settings-section-heading">
        <h3>可用备份</h3>
        <span>最多保留 7 份</span>
      </div>
      <div class="backup-list">
        ${
          backups.length
            ? backups
                .map(
                  (backup) => `
                    <article>
                      <span class="backup-check">${icon("check")}</span>
                      <div>
                        <strong>${escapeHtml(backup.name)}</strong>
                        <span>${new Date(backup.createdAt).toLocaleString("zh-CN")} · ${formatBytes(backup.size)}</span>
                      </div>
                      <button class="secondary-button" data-restore="${escapeHtml(backup.name)}">恢复</button>
                    </article>
                  `,
                )
                .join("")
            : `<div class="backup-empty">${icon("backup")}<p>还没有备份。创建第一份备份后会显示在这里。</p></div>`
        }
      </div>
      <div class="settings-section-heading agent-settings-heading">
        <div><h3>Agent 运行时</h3><span>账号、模型和密钥由各 CLI 自己管理</span></div>
        <button class="secondary-button" id="addAgentRuntime">${icon("plus")}<span>添加 CLI</span></button>
      </div>
      <div class="agent-runtime-list">
        ${runtimes
          .map(
            (runtime) => `
              <article>
                <span class="runtime-icon">${icon("terminal")}</span>
                <div>
                  <strong>${escapeHtml(runtime.name)}</strong>
                  <span title="${escapeHtml(runtime.executable ?? "")}">${escapeHtml(runtime.version ?? runtime.executable ?? runtime.detail ?? "未安装")}</span>
                </div>
                <span class="runtime-status ${runtime.available ? "available" : "unavailable"}">${runtime.available ? "可用" : "不可用"}</span>
                ${runtime.builtIn ? "" : `<button class="secondary-button" data-delete-runtime="${escapeHtml(runtime.id)}">删除</button>`}
              </article>`,
          )
          .join("")}
      </div>
    `);
    document.querySelector("#backupNow")?.addEventListener("click", (event) =>
      runWithButton(event.currentTarget as HTMLButtonElement, async () => {
        await api<Backup>("/api/backups", { method: "POST", body: "{}" });
        closeModal();
        await showSettings();
        toast("备份已创建");
      }),
    );
    document.querySelectorAll<HTMLButtonElement>("[data-restore]").forEach((button) => {
      button.addEventListener("click", () => {
        const name = button.dataset.restore;
        if (!name) return;
        showModal(
          `
            <div class="modal-symbol danger">${icon("backup")}</div>
            <p class="modal-kicker">整库替换</p>
            <h2 id="modalTitle">从这个备份恢复？</h2>
            <p>当前数据库会先自动备份，然后由 <strong>${escapeHtml(name)}</strong> 整体替换。</p>
            <div class="modal-actions">
              <button class="secondary-button" data-close>取消</button>
              <button class="danger-button" id="confirmRestore" data-autofocus>确认恢复</button>
            </div>
          `,
          "compact",
        );
        document.querySelector("#confirmRestore")?.addEventListener("click", (event) =>
          runWithButton(event.currentTarget as HTMLButtonElement, async () => {
            await api<void>(`/api/backups/${encodeURIComponent(name)}/restore`, {
              method: "POST",
              body: "{}",
            });
            data = await api<Bootstrap>("/api/bootstrap");
            closeModal();
            render();
            toast("阅读数据已恢复");
          }),
        );
      });
    });
    document.querySelector("#addAgentRuntime")?.addEventListener("click", showAddAgentRuntime);
    document.querySelectorAll<HTMLButtonElement>("[data-delete-runtime]").forEach((button) => {
      button.addEventListener("click", () => {
        const runtimeId = button.dataset.deleteRuntime;
        if (!runtimeId) return;
        void runWithButton(button, async () => {
          await api<void>(`/api/agent/runtimes/${encodeURIComponent(runtimeId)}`, {
            method: "DELETE",
            body: "{}",
          });
          closeModal();
          await showSettings();
          toast("自定义 Agent CLI 已删除");
        });
      });
    });
  });
}

function showAddAgentRuntime(): void {
  showModal(`
    <div class="modal-header">
      <div>
        <p class="modal-kicker">自定义 Agent 运行时</p>
        <h2 id="modalTitle">添加本机 CLI</h2>
      </div>
      <button class="modal-close" data-close aria-label="关闭">${icon("close")}</button>
    </div>
    <form class="agent-runtime-form" id="agentRuntimeForm">
      <label><span>显示名称</span><input id="agentRuntimeName" required maxlength="80" placeholder="例如：My Agent" /></label>
      <label><span>可执行文件绝对路径</span><input id="agentRuntimePath" required placeholder="/opt/homebrew/bin/my-agent" /></label>
      <label><span>启动参数 <small>每行一个，不经过 shell</small></span><textarea id="agentRuntimeArguments" rows="4" placeholder="--print&#10;--output-format&#10;text"></textarea></label>
      <p>GoodReader 会通过标准输入传递任务，并从标准输出读取回答。模型和登录配置仍由这个 CLI 自己管理。</p>
      <div class="modal-actions"><button type="button" class="secondary-button" data-close>取消</button><button type="submit" class="primary-button">添加 CLI</button></div>
    </form>
  `);
  document.querySelector<HTMLFormElement>("#agentRuntimeForm")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const submit = (event.currentTarget as HTMLFormElement).querySelector<HTMLButtonElement>('[type="submit"]');
    if (!submit) return;
    void runWithButton(submit, async () => {
      const name = document.querySelector<HTMLInputElement>("#agentRuntimeName")?.value.trim() ?? "";
      const executable = document.querySelector<HTMLInputElement>("#agentRuntimePath")?.value.trim() ?? "";
      const argumentsValue = document.querySelector<HTMLTextAreaElement>("#agentRuntimeArguments")?.value ?? "";
      const argumentsList = argumentsValue.split("\n").map((value) => value.trim()).filter(Boolean);
      await api<AgentRuntime[]>("/api/agent/runtimes", {
        method: "POST",
        body: JSON.stringify({ name, executable, arguments: argumentsList }),
      });
      closeModal();
      await showSettings();
      toast("自定义 Agent CLI 已添加");
    });
  });
}

function showModal(content: string, size = "wide"): void {
  const modalRoot = document.querySelector<HTMLDivElement>("#modalRoot");
  if (!modalRoot) return;
  if (!modalReturnFocus) modalReturnFocus = document.activeElement as HTMLElement | null;
  modalRoot.innerHTML = `
    <div class="modal-backdrop">
      <section class="modal-card ${size}" role="dialog" aria-modal="true" aria-labelledby="modalTitle">
        ${content}
      </section>
    </div>
  `;
  const backdrop = modalRoot.querySelector<HTMLElement>(".modal-backdrop");
  modalRoot.querySelectorAll("[data-close]").forEach((button) => {
    button.addEventListener("click", closeModal);
  });
  backdrop?.addEventListener("pointerdown", (event) => {
    if (event.target === event.currentTarget) closeModal();
  });
  backdrop?.addEventListener("keydown", trapModalFocus);
  window.setTimeout(() => {
    modalRoot.querySelector<HTMLElement>("[data-autofocus], .modal-close, button")?.focus();
  });
}

function trapModalFocus(event: KeyboardEvent): void {
  if (event.key === "Escape") {
    event.preventDefault();
    closeModal();
    return;
  }
  if (event.key !== "Tab") return;
  const modal = document.querySelector<HTMLElement>(".modal-card");
  const focusable = [...(modal?.querySelectorAll<HTMLElement>("button, [href], input, textarea, select, [tabindex]:not([tabindex='-1'])") ?? [])]
    .filter((element) => !element.hasAttribute("disabled"));
  if (!focusable.length) return;
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

function closeModal(): void {
  const modalRoot = document.querySelector<HTMLDivElement>("#modalRoot");
  if (modalRoot) modalRoot.innerHTML = "";
  const target = modalReturnFocus;
  modalReturnFocus = null;
  target?.focus();
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function toast(message: string): void {
  const element = document.querySelector<HTMLDivElement>("#toast");
  if (!element) return;
  element.textContent = message;
  element.classList.add("visible");
  window.setTimeout(() => element.classList.remove("visible"), 3200);
}

async function runWithButton(button: HTMLButtonElement, action: () => Promise<void>): Promise<void> {
  if (button.disabled) return;
  button.disabled = true;
  button.classList.add("loading");
  button.setAttribute("aria-busy", "true");
  try {
    await action();
  } catch (error) {
    toast(error instanceof Error ? error.message : "操作失败");
  } finally {
    if (button.isConnected) {
      button.disabled = false;
      button.classList.remove("loading");
      button.removeAttribute("aria-busy");
    }
  }
}

async function run(action: () => Promise<void>): Promise<void> {
  try {
    await action();
  } catch (error) {
    toast(error instanceof Error ? error.message : "操作失败");
  }
}

function renderLoading(): void {
  root.innerHTML = `
    <div class="app-loading" role="status" aria-live="polite">
      ${brandMark()}
      <strong>正在打开书架</strong>
      <span>读取本地书籍与阅读进度…</span>
      <i></i>
    </div>
  `;
}

async function boot(): Promise<void> {
  renderLoading();
  try {
    const [bootstrap, tasks] = await Promise.all([
      api<Bootstrap>("/api/bootstrap"),
      api<ImportTask[]>("/api/import/tasks"),
    ]);
    data = bootstrap;
    currentImportTask = tasks.find(isUnfinishedImport) ?? null;
    render();
    scheduleCurrentImportPoll();
  } catch (error) {
    root.innerHTML = `
      <main class="fatal-screen">
        ${brandMark()}
        <h1>书架暂时无法打开</h1>
        <p>${escapeHtml(error instanceof Error ? error.message : "未知错误")}</p>
        <button class="primary-button" id="retryBoot">${icon("refresh")}<span>重新加载</span></button>
      </main>
    `;
    document.querySelector("#retryBoot")?.addEventListener("click", () => window.location.reload());
  }
}

void boot();
