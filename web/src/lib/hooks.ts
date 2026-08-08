import { useEffect, useState } from "react";

/** Panel preferences outlive a reload — a console someone keeps open on a
 * second monitor should not re-open both sidebars every time smith restarts. */
export function usePreference(key: string, fallback: boolean) {
  const [value, setValue] = useState<boolean>(() => {
    const stored = window.localStorage.getItem(key);
    return stored === null ? fallback : stored === "1";
  });
  useEffect(() => {
    window.localStorage.setItem(key, value ? "1" : "0");
  }, [key, value]);
  return [value, () => setValue((previous) => !previous)] as const;
}

/** Whether a CSS media query currently matches.
 *
 * The stats sidebar is hidden below `xl` by a Tailwind class, and the top bar
 * mirrors its two headline numbers when it is not on screen. Those two
 * decisions have to agree, so the breakpoint is read here rather than
 * assumed — otherwise a narrow window loses the panel *and* the mirrors. */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const list = window.matchMedia(query);
    const update = () => setMatches(list.matches);
    update();
    list.addEventListener("change", update);
    return () => list.removeEventListener("change", update);
  }, [query]);
  return matches;
}
