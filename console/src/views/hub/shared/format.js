/**
 * 控制中台各页面共用的展示格式化。
 *
 * 放在这里而不是各页面各写一份：时间与状态的呈现方式不一致会让人以为是不同的东西，
 * 尤其在排障时——「8 秒前」和「2026-09-17 11:20:03」并排出现，读的人要多花一秒才
 * 反应过来是同一个字段。
 */

function toDate(value) {
  if (!value) return null;
  const date = value instanceof Date ? value : new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

const pad = (n, width = 2) => String(n).padStart(width, "0");

/** 绝对时间：`2026-09-17 11:20:03` */
export function formatTime(value) {
  const date = toDate(value);
  if (!date) return "—";
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
}

/** 只到秒的时间戳（日志类展示用）：`11:20:03.123` */
export function formatClock(value) {
  const date = toDate(value);
  if (!date) return "—";
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(
    date.getSeconds(),
  )}.${pad(date.getMilliseconds(), 3)}`;
}

/**
 * 相对时间：`8 秒前`。
 *
 * 实例心跳与执行记录靠它判读新鲜度——绝对时间要人自己减一次才知道是不是刚发生。
 * `now` 可传入以便同一页面上多行的基准一致（否则同屏两行的时间差会被渲染耗时吃掉）。
 */
export function fromNow(value, now = Date.now()) {
  const date = toDate(value);
  if (!date) return "—";

  const diff = now - date.getTime();
  // 时钟回拨或刚好同一毫秒时不该显示「-1 秒前」
  if (diff < 1000) return "刚刚";
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds} 秒前`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  return `${days} 天前`;
}

/** 毫秒耗时：小于 1 秒给毫秒，否则给秒并保留两位 */
export function formatDuration(ms) {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

/**
 * 用起止时间算耗时。
 *
 * **没跑完时返回 `—` 而不是「到目前为止的耗时」**：后者会让人以为这次执行
 * 已经结束了、只是慢，而它其实还在跑（或者卡住了）——这两件事的处置完全不同。
 */
export function elapsedBetween(start, end) {
  if (!start || !end) return "—";
  const from = new Date(start).getTime();
  const to = new Date(end).getTime();
  if (Number.isNaN(from) || Number.isNaN(to)) return "—";
  return formatDuration(to - from);
}

/**
 * 中文标签（部分状态值直接来自中台的英文枚举）。
 *
 * 只翻译有确定含义的；不认识的**原样返回**而不是硬套一个词——
 * 把没见过的状态显示成「未知」会把一个真问题藏起来。
 */
const STATUS_TEXT = {
  succeeded: "成功",
  failed: "失败",
  rejected: "被拒",
  running: "执行中",
  queued: "排队中",
  skipped: "跳过",
  healthy: "健康",
  ok: "正常",
  error: "错误",
  online: "在线",
  offline: "离线",
  draft: "草稿",
  published: "已发布",
  archived: "已归档",
};

export function statusText(status) {
  if (!status) return "—";
  return STATUS_TEXT[status] || status;
}

/** Element Plus 的 tag 类型：成功绿 / 失败红 / 被拒橙 / 进行中蓝 / 其余灰 */
export function statusTagType(status) {
  switch (status) {
    case "succeeded":
    case "ok":
    case "healthy":
    case "online":
    case "published":
      return "success";
    case "failed":
    case "error":
      return "danger";
    case "rejected":
      return "warning";
    case "running":
    case "queued":
      return "primary";
    default:
      return "info";
  }
}
