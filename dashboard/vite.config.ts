import { defineConfig, loadEnv } from "vite";
import preact from "@preact/preset-vite";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig(({ mode }) => ({
  plugins: [preact(), viteSingleFile({ removeViteModuleLoader: true })],
  build: {
    target: "es2022",
    assetsInlineLimit: 100_000_000,
    cssCodeSplit: false,
    reportCompressedSize: false,
    chunkSizeWarningLimit: 100_000,
  },
  server: {
    proxy: {
      "/analytics": {
        target: loadEnv(mode, process.cwd(), "").RELAY_URL || "http://127.0.0.1:8080",
      },
    },
  },
}));
