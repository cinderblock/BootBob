// Times are shown local first; UTC is one hover (or tap) away in the title.

const local = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "numeric",
  hour: "numeric",
  minute: "2-digit",
  second: "2-digit",
  timeZoneName: "short",
});

/** `Oct 23, 2026, 3:36:05 PM PDT (22:36:05Z)` */
export function fullTime(ms: number): string {
  const d = new Date(ms);
  return `${local.format(d)} (${d.toISOString().slice(11, 19)}Z)`;
}

/** `just now`, `12 s ago`, `5 min ago`, `3 h ago`, `2 days ago`. */
export function ago(ms: number, now: number = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 3) return "just now";
  if (s < 60) return `${s} s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h} h ago`;
  return `${Math.round(h / 24)} days ago`;
}
