import { defineConfig } from "vite";
import { resolve } from "node:path";

export default defineConfig({
  root: resolve(import.meta.dirname, "frontend"),
  publicDir: resolve(import.meta.dirname, "frontend/public"),
  build: {
    outDir: resolve(import.meta.dirname, "frontend/dist"),
    emptyOutDir: true,
    rollupOptions: {
      input: {
        app: resolve(import.meta.dirname, "frontend/index.html"),
        reader: resolve(import.meta.dirname, "frontend/src/reader.ts")
      },
      output: {
        entryFileNames: "assets/[name].js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]"
      }
    }
  }
});
