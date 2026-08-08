import { cn } from "@/lib/utils";

export function Card({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("panel px-4 py-3", className)} {...props} />;
}

/** A titled block of the stats sidebar. The eyebrow is a `<h3>` because the
 * panel is a real document outline for a screen reader, not decoration. */
export function Section({
  title,
  action,
  className,
  children,
}: {
  title: string;
  action?: React.ReactNode;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <section className={cn("flex flex-col gap-2", className)}>
      <div className="flex items-center justify-between gap-2">
        <h3 className="eyebrow">{title}</h3>
        {action}
      </div>
      {children}
    </section>
  );
}

/** One label/value line. Values are mono and tabular so a counter ticking
 * upward never nudges its own label. */
export function Stat({
  label,
  value,
  hint,
  tone = "text-text",
}: {
  label: string;
  value: React.ReactNode;
  hint?: string;
  tone?: string;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3 text-xs">
      <span className="truncate text-secondary">{label}</span>
      <span className={cn("font-mono tabular", tone)} title={hint}>
        {value}
      </span>
    </div>
  );
}

export function Separator({ className }: { className?: string }) {
  return <hr className={cn("border-0 border-t border-text/8", className)} />;
}
