import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  root: "src",
  plugins: [react()],
  base: "./",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks(id) {
          const normalized = id.replace(/\\/g, "/");
          if (!normalized.includes("/node_modules/")) return undefined;
          if (
            normalized.includes("/node_modules/react-dom/") ||
            normalized.includes("/node_modules/react/") ||
            normalized.includes("/node_modules/scheduler/")
          ) {
            return "vendor-react";
          }
          if (normalized.includes("/node_modules/@radix-ui/")) {
            return "vendor-radix";
          }
          if (normalized.includes("/node_modules/@tanstack/")) {
            return "vendor-tanstack";
          }
          if (
            normalized.includes("/node_modules/i18next/") ||
            normalized.includes("/node_modules/react-i18next/")
          ) {
            return "vendor-i18n";
          }
          if (normalized.includes("/node_modules/lucide-react/")) {
            return "vendor-icons";
          }
          return "vendor";
        },
      },
    },
  },
  server: {
    port: 3000,
    strictPort: true,
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_"],
});
