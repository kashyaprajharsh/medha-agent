import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  build: {
    rollupOptions: {
      output: { manualChunks: { syntax: ["highlight.js/lib/common"] } },
    },
  },
  server: {
    host: "127.0.0.1",
    strictPort: true,
    port: 1420,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
