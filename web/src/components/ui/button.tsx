// shadcn/ui-style primitive: CVA variants, cn() merge, Ember roles.
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-1.5 rounded-md border text-sm px-3 py-1.5 " +
    "cursor-pointer transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
  {
    variants: {
      variant: {
        default: "border-ember text-ember bg-overlay hover:bg-hover",
        success: "border-success text-success bg-overlay hover:bg-hover",
        danger: "border-danger text-danger bg-overlay hover:bg-hover",
        warning: "border-warning text-warning bg-overlay hover:bg-hover",
        ghost: "border-transparent text-secondary hover:text-text hover:bg-hover",
      },
      size: {
        default: "",
        sm: "px-2 py-1 text-xs",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

export function Button({
  className,
  variant,
  size,
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement> &
  VariantProps<typeof buttonVariants>) {
  return (
    <button className={cn(buttonVariants({ variant, size }), className)} {...props} />
  );
}
