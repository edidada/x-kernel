import { defineConfig } from "vite";
import { contentPlugin } from "./tooling/content.ts";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react(), contentPlugin()],
  preview: {
    host: "0.0.0.0",
    port: 56111,
    strictPort: true,
    allowedHosts: ["a.debin.cc", "a.luodeb.top"],
  },
  server: {
    host: "0.0.0.0",
    port: 56111,
    strictPort: true,
    allowedHosts: ["a.debin.cc", "a.luodeb.top"],
  },
});
