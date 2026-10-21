import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api/portal": "http://127.0.0.1:8787",
      // The live socket: the daemon checks Origin against this dev host.
      "/api/live": { target: "ws://127.0.0.1:8787", ws: true },
      "/api": "http://127.0.0.1:8787",
      "/invite": "http://127.0.0.1:8787",
      "/health": "http://127.0.0.1:8787",
      "/status": "http://127.0.0.1:8787",
      "/jobs": "http://127.0.0.1:8787",
      "/scan": "http://127.0.0.1:8787",
      "/acquire": "http://127.0.0.1:8787",
    },
  },
});
