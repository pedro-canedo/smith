import { cn } from "@/lib/utils";

export function Input({
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "w-full rounded-md border border-disabled bg-overlay px-3 py-2 text-sm",
        "placeholder:text-disabled focus:border-ember focus:outline-none",
        className,
      )}
      {...props}
    />
  );
}
