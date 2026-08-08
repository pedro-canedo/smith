// Formatters shared by the rail, the stats panel and the board. One place,
// because a token count rendered two ways in two panels reads as two numbers.

/** 1_234 → "1.2k". Counters here are compared at a glance, not audited. */
export function compact(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** Sub-cent spend still deserves digits: a session that has cost $0.0004 is
 * meaningfully different from one that has cost nothing. */
export function money(usd: number): string {
  if (usd === 0) return "$0";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(2)}`;
}

/** Elapsed wall clock, coarsest useful unit. */
export function duration(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

/** How long ago, for board cards and the session list. */
export function ago(timestampMs: number): string {
  const seconds = Math.max(0, (Date.now() - timestampMs) / 1000);
  if (seconds < 90) return "now";
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.round(seconds / 3600)}h`;
  return `${Math.round(seconds / 86_400)}d`;
}

/** A session id is a UUID; the rail has room for a handle, not for 36
 * characters. The full value is always available on the title attribute and
 * the copy button. */
export function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…` : id;
}

/** The last two path segments — enough to tell two projects apart without
 * spending a whole line on someone's home directory. */
export function shortPath(path: string): string {
  const parts = path.split(/[/\\]/).filter(Boolean);
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join("/")}`;
}
