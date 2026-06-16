import { defineConfig } from "astro/config";

// Static takeover UI, served by the browser-session-takeover daemon out of its
// built ./dist (TAKEOVER_WEBROOT). The daemon serves the same index.html for
// every /takeover/<token>; the client reads the token from location.pathname.
export default defineConfig({
  build: {
    // Fold the (small) CSS into <head> so there's one fewer asset request; the
    // only emitted asset is the JS bundle under /_astro/.
    inlineStylesheets: "always",
  },
});
