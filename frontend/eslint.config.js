import js from "@eslint/js";
import globals from "globals";
import tseslint from "typescript-eslint";
import { configs as astroConfigs } from "eslint-plugin-astro";
import { defineConfig, globalIgnores } from "eslint/config";

export default defineConfig([
  globalIgnores(["dist", ".astro", "node_modules"]),
  js.configs.recommended,
  tseslint.configs.recommended,
  astroConfigs.recommended,
  {
    files: ["**/*.{ts,mts,cts,js,mjs,cjs,astro}"],
    languageOptions: {
      ecmaVersion: "latest",
      globals: { ...globals.browser, ...globals.node },
    },
    rules: {
      // DOM/CDP glue: untyped CDP payloads and intentional empty catches.
      "@typescript-eslint/no-explicit-any": "off",
      "no-empty": ["error", { allowEmptyCatch: true }],
    },
  },
]);
