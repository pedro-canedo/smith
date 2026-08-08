import { cn } from "@/lib/utils";

/** How full is too full. The thresholds match what the TUI's context gauge
 * warns at, so the two frontends never disagree about whether a session is
 * in trouble. */
export function occupancyTone(fraction: number): string {
  if (fraction >= 0.9) return "text-danger";
  if (fraction >= 0.7) return "text-amber";
  return "text-ember";
}

/** A horizontal bar. `segments` render left to right and are expected to sum
 * to at most `total`; the remainder is the track. */
export function Meter({
  segments,
  total,
  className,
}: {
  segments: { value: number; className: string; label?: string }[];
  total: number;
  className?: string;
}) {
  const safe = Math.max(total, 1);
  return (
    <div
      className={cn(
        "flex h-1.5 w-full overflow-hidden rounded-full bg-overlay",
        className,
      )}
    >
      {segments.map((segment, index) => (
        <div
          key={index}
          title={segment.label}
          className={cn("h-full first:rounded-l-full last:rounded-r-full", segment.className)}
          style={{ width: `${Math.min(100, (segment.value / safe) * 100)}%` }}
        />
      ))}
    </div>
  );
}

/** The context gauge: a ring, because it is the one number on the page worth
 * a shape of its own — everything else is a row in a list. */
export function Ring({
  fraction,
  label,
  caption,
  tone,
  size = 92,
}: {
  fraction: number;
  label: string;
  caption?: string;
  tone: string;
  size?: number;
}) {
  const stroke = 7;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  const clamped = Math.min(1, Math.max(0, fraction));

  return (
    <div className="relative shrink-0" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90">
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          strokeWidth={stroke}
          className="stroke-overlay"
        />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={circumference}
          strokeDashoffset={circumference * (1 - clamped)}
          className={cn("stroke-current transition-[stroke-dashoffset] duration-500", tone)}
        />
      </svg>
      <div className="absolute inset-0 flex flex-col items-center justify-center">
        <span className={cn("font-mono text-lg leading-none tabular", tone)}>{label}</span>
        {caption && (
          <span className="mt-1 font-mono text-[0.625rem] text-disabled tabular">
            {caption}
          </span>
        )}
      </div>
    </div>
  );
}
