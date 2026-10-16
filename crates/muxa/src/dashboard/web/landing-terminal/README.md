# Browser terminal dependencies

Vendored ESM distributions from the official npm packages:

- `@xterm/xterm` 6.0.0: `lib/xterm.mjs`, `css/xterm.css`, MIT license.
- `@xterm/addon-fit` 0.11.0: `lib/addon-fit.mjs`, MIT license.

The only modification is removing source-map URL comments (maps are not shipped).
No CDN requests or package installation are needed to run the site. Both the
daemon's embedded static assets and GitHub Pages serve these files locally.

The terminal renders and collects input. `landing-shell.mjs` interprets the
browser-local demo commands; there is no host shell or remote execution endpoint.
