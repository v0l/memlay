export const fmt = (n: number | null | undefined) => (n ?? 0).toLocaleString();

export function fmtUs(us: number | null | undefined): string {
  if (us == null) return "-";
  if (us >= 1e6) return `${(us / 1e6).toFixed(2)} s`;
  if (us >= 1e3) return `${(us / 1e3).toFixed(1)} ms`;
  return `${Math.round(us)} us`;
}

export const fmtMs = (ms: number) => fmtUs(ms * 1000);

export function fmtBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let i = -1;
  let b = bytes;
  do {
    b /= 1024;
    i++;
  } while (b >= 1024 && i < units.length - 1);
  return `${b.toFixed(1)} ${units[i]}`;
}

export function fmtDur(s: number): string {
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}
