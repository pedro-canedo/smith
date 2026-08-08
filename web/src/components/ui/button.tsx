// shadcn/ui-style primitive: CVA variants, cn() merge, Ember roles.
//
// Hand-rolled rather than pulled from Radix, like every primitive here. The
// whole app is include_str!'d into the smith binary as one file, so bytes are
// a permanent cost paid by every user; these components need no portal, no
// focus trap and no collision detection, which is most of what Radix is for.
// The API shape is shadcn's, so a component that does need it can be dropped
// in later without a rewrite.
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex shrink-0 items-center justify-center gap-1.5 rounded-lg border font-medium " +
    "cursor-pointer transition-[background-color,border-color,color,opacity] duration-150 " +
    "disabled:pointer-events-none disabled:opacity-40 " +
    "[&_svg]:pointer-events-none [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        // The one filled button on screen at a time: send, confirm, resume.
        primary:
          "border-transparent bg-ember text-base hover:bg-amber shadow-[0_1px_12px_-4px_var(--color-ember)]",
        default: "border-text/10 bg-overlay/70 text-text hover:border-text/20 hover:bg-hover",
        success: "border-success/40 bg-success/10 text-success hover:bg-success/20",
        danger: "border-danger/40 bg-danger/10 text-danger hover:bg-danger/20",
        warning: "border-warning/40 bg-warning/10 text-warning hover:bg-warning/20",
        info: "border-info/40 bg-info/10 text-info hover:bg-info/20",
        ghost: "border-transparent text-secondary hover:bg-hover hover:text-text",
      },
      size: {
        default: "h-9 px-3.5 text-sm [&_svg]:size-4",
        sm: "h-7 px-2.5 text-xs [&_svg]:size-3.5",
        icon: "size-9 [&_svg]:size-4",
        "icon-sm": "size-7 [&_svg]:size-3.5",
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

export { buttonVariants };
