import { readFileSync } from "node:fs";

const source = readFileSync(new URL("../frontend/src/main.ts", import.meta.url), "utf8");
const start = source.indexOf("function showImportTaskProgress");
const end = source.indexOf("function bindImportTaskActions", start);

if (start < 0 || end < 0) {
  throw new Error("没有找到导入进度窗口实现");
}

const progressImplementation = source.slice(start, end);
if (!progressImplementation.includes("updateImportTaskProgress")) {
  throw new Error("导入进度轮询仍未使用原位更新，窗口可能被重复创建");
}
if (progressImplementation.includes("pollImportTask(initial.id, renderTask)")) {
  throw new Error("轮询仍会调用整窗重绘函数，导致动画、焦点与滚动位置反复重置");
}
if (!source.includes('id="currentImportProgress"')) {
  throw new Error("主界面侧边栏缺少当前导入进度入口");
}
if (!source.includes('id="importTaskDetailsToggle"') || !source.includes('aria-expanded="false"')) {
  throw new Error("生成框缺少可访问的进度详情折叠按钮");
}
if (!source.includes("/events")) {
  throw new Error("进度详情没有读取后端持久化事件");
}
if (!source.includes("expandedImportTaskIds")) {
  throw new Error("详情展开状态不会在轮询刷新之间保持");
}
if (!source.includes("afterSeq=") || !source.includes("importTaskEventCache")) {
  throw new Error("生成详情仍在重复读取全部事件，没有使用增量序号");
}
if (!source.includes("importTaskDetailsSummary") || !source.includes("预计剩余")) {
  throw new Error("生成详情缺少正文块、耗时和 ETA 摘要");
}
if (!source.includes('event.state === "retrying"') || !source.includes('items.push("自动重试")')) {
  throw new Error("生成详情没有向用户标识后台自动重试状态");
}
if (!source.includes('name="pdfMode" value="auto"')
  || !source.includes('name="pdfMode" value="text-layer"')
  || !source.includes('name="pdfMode" value="ocr"')) {
  throw new Error("PDF 导入缺少自动、文本层和 OCR 三种明确模式");
}
if (!source.includes("body: JSON.stringify({ kind: \"pdf\", pdfMode })")) {
  throw new Error("PDF 导入模式没有传递到后端预检");
}
if (!source.includes('const needsLayoutAgent = preflight.kind === "pdf"')
  || !source.includes("needsLayoutAgent || translate?.checked")) {
  throw new Error("PDF 导入没有强制选择逐页排版 Agent");
}
if (!source.includes("PDF 制书当前不支持翻译")
  || !source.includes("needsLayoutAgent || !availableRuntimes.length")) {
  throw new Error("PDF 导入应禁用「翻译为简体中文」并说明不支持");
}
if (!source.includes("逐页调用 Agent 恢复阅读顺序、书籍排版和完整图片区域")) {
  throw new Error("PDF 导入没有向用户说明逐页 Agent 排版工作量");
}

console.log("导入进度窗口稳定刷新，PDF 模式选择、逐页 Agent 排版与增量详情均已接通");
