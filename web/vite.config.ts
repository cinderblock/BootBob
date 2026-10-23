// The web UI, written against React's API and built against preact/compat (a ~4 KB
// runtime), like BenchBob's.
//
// Dev: `bun run dev` serves it with hot reload; BOOTBOB_PROXY=http://host:8067 proxies
// /api, /boot and /theme.css to a running BootBob.
import { loadEnv } from "vite";
import { defineConfig } from "vitest/config";
import { fileURLToPath } from "node:url";

const here = (p: string) => fileURLToPath(new URL(p, import.meta.url));

export default defineConfig(({ mode }) => {
  const proxy = loadEnv(mode, here("."), "").BOOTBOB_PROXY;
  return {
    base: "./",
    resolve: {
      alias: [
        { find: /^react$/, replacement: "preact/compat" },
        { find: /^react-dom$/, replacement: "preact/compat" },
        { find: /^react-dom\/client$/, replacement: "preact/compat/client" },
        { find: /^react\/jsx-runtime$/, replacement: "preact/jsx-runtime" },
        { find: /^react\/jsx-dev-runtime$/, replacement: "preact/jsx-runtime" },
      ],
    },
    build: { outDir: "dist", emptyOutDir: true, target: "es2022", reportCompressedSize: false },
    server: proxy
      ? { proxy: Object.fromEntries(["/api", "/boot", "/theme.css"].map((p) => [p, { target: proxy, changeOrigin: true }])) }
      : undefined,
    test: { environment: "happy-dom", include: ["src/**/*.test.{ts,tsx}"] },
  };
});
