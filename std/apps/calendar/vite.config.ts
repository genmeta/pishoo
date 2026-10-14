import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  base: "./",
  plugins: [solid()],
  server: {
    proxy: {
      "^/std/calendar/(events|event)(\\?|$)":
        process.env.PISHOO_CALENDAR_BACKEND ?? "http://127.0.0.1:3000",
    },
  },
});
