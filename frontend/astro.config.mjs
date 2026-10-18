import { defineConfig } from "astro/config";

// The running takeover daemon, for `pnpm dev` (see the dev server note below).
const DAEMON = process.env.TAKEOVER_DAEMON ?? "http://127.0.0.1:9223";

// Static takeover UI, served by the browser-session-takeover daemon out of its
// built ./dist (TAKEOVER_WEBROOT). The daemon serves the same index.html for
// every /takeover/<token>; the client reads the token from location.pathname.
export default defineConfig({
  // Astro's floating dev toolbar would sit over the screencast in `pnpm dev`.
  devToolbar: { enabled: false },
  build: {
    // Fold the (small) CSS into <head> so there's one fewer asset request; the
    // only emitted asset is the JS bundle under /_astro/.
    inlineStylesheets: "always",
  },
  // `pnpm dev` against real sessions: open a minted link with its origin
  // swapped for the dev server's. The page itself comes from here (hot
  // reload); its /claim, /done and /capture calls go to the daemon.
  vite: {
    server: {
      proxy: {
        "^/takeover/[0-9a-f]{32}/": { target: DAEMON },
      },
    },
    plugins: [
      {
        name: "takeover-page",
        configureServer(server) {
          server.middlewares.use((req, _res, next) => {
            if (req.url && /^\/takeover\/[0-9a-f]{32}\/?(\?|$)/.test(req.url))
              req.url = "/";
            next();
          });
        },
      },
    ],
  },
});
