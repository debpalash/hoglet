import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Build to web/dist, which rust-embed compiles into the binary. Relative base
// so hashed asset URLs resolve wherever the binary is mounted.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true },
});
