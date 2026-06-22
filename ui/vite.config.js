import { defineConfig } from "vite";

// Tauri expects a fixed dev port and the built assets in ../dist relative to
// src-tauri (configured as `frontendDist` there). Keep clearScreen off so
// Rust compiler output stays visible during `tauri dev`.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2021",
  },
});
