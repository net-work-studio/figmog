import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";

// No @vitejs/plugin-react: Vite transforms JSX itself (automatic runtime);
// the plugin only adds Fast Refresh, which a production build never uses.
export default defineConfig({
  plugins: [tailwindcss()],
});
