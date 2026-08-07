import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-xs font-medium",
  {
    variants: {
      variant: {
        default: "bg-overlay text-secondary",
        ember: "bg-overlay text-ember",
        success: "bg-overlay text-success",
        danger: "bg-overlay text-danger",
        warning: "bg-overlay text-warning",
        info: "bg-overlay text-info",
        amber: "bg-overlay text-amber",
        plan: "bg-overlay text-plan",
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
