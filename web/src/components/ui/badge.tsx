import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-[0.6875rem] " +
    "font-medium leading-4 whitespace-nowrap [&_svg]:size-3 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default: "border-text/8 bg-overlay/70 text-secondary",
        ember: "border-ember/30 bg-ember/10 text-ember",
        success: "border-success/30 bg-success/10 text-success",
        danger: "border-danger/30 bg-danger/10 text-danger",
        warning: "border-warning/30 bg-warning/10 text-warning",
        info: "border-info/30 bg-info/10 text-info",
        amber: "border-amber/30 bg-amber/10 text-amber",
        plan: "border-plan/30 bg-plan/10 text-plan",
        outline: "border-text/12 bg-transparent text-disabled",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

export function Badge({
  className,
  variant,
  ...props
}: React.HTMLAttributes<HTMLSpanElement> & VariantProps<typeof badgeVariants>) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}

/** A 6px status dot. `pulse` is for state that is actively changing — a
 * running turn, a pending approval — never for a resting colour. */
export function Dot({
  className,
  pulse,
}: {
  className?: string;
  pulse?: boolean;
}) {
  return (
    <span
      className={cn(
        "inline-block size-1.5 shrink-0 rounded-full bg-current",
        pulse && "breathe",
        className,
      )}
    />
  );
}
