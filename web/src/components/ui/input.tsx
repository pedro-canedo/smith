import { cn } from "@/lib/utils";

const field =
  "w-full rounded-lg border border-text/10 bg-overlay/60 px-3 py-2 text-sm text-text " +
  "placeholder:text-disabled transition-colors focus:border-ember/60 focus:bg-overlay " +
  "focus:outline-none";

export function Input({
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input className={cn(field, className)} {...props} />;
}

/** The composer's field. A textarea rather than an input because a prompt is
 * routinely a paragraph, and the one thing a browser can offer over the TUI's
 * fixed box is growing to fit what you actually typed. */
export function Textarea({
  className,
  ...props
}: React.TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      className={cn(field, "resize-none leading-relaxed", className)}
      {...props}
    />
  );
}
