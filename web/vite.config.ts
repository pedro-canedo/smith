import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { viteSingleFile } from "vite-plugin-singlefile";

// Single-file on purpose: the Rust server embeds exactly one artifact
// (`web/dist/index.html`) via include_str!, the same posture as
// webconfig/ui.html — no asset resolver on a privileged socket, zero static
// routes in the whitelist. See docs/web-console.md §6.
export default defineConfig({
  plugins: [react(), tailwindcss(), viteSingleFile()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  server: {
    // Dev loop: `pnpm dev` against a running `smith --web`. The token comes
    // from the printed URL via VITE_SMITH_TOKEN; the port via
    // VITE_SMITH_PORT.
    proxy: {
      "/api": {
        target: `http://127.0.0.1:${process.env.VITE_SMITH_PORT ?? "8420"}`,
        changeOrigin: false,
      },
    },
  },
});
