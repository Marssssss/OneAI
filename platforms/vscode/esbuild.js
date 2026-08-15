// esbuild bundler — bundles src/extension.ts → dist/extension.js (single file,
// no runtime node_modules needed in the .vsix). The webview HTML/JS is shipped
// as static assets (src/webview/*) and referenced via vscode-webview URIs.
const esbuild = require("esbuild");
const fs = require("fs");
const path = require("path");

// Re-sync the shared scenario editor into the webview root. The webview can
// only load scripts under src/webview/ (its localResourceRoots), so the
// canonical source in platforms/shared/ is copied here on every compile —
// this keeps the committed copy from drifting. (CI also checks via
// platforms/shared/sync.sh.) No-op if the source is unchanged.
try {
  const src = path.resolve(__dirname, "../shared/scenario-editor.js");
  const dest = path.resolve(__dirname, "src/webview/scenario-editor.js");
  if (fs.existsSync(src)) fs.copyFileSync(src, dest);
} catch (e) {
  console.warn("[oneai] scenario-editor sync skipped:", e.message);
}

const watch = process.argv.includes("--watch");

/** @type {esbuild.BuildOptions} */
const options = {
  entryPoints: ["src/extension.ts"],
  bundle: true,
  outfile: "dist/extension.js",
  external: ["vscode"], // provided by the extension host at runtime
  format: "cjs",
  platform: "node",
  target: "node18",
  sourcemap: true,
  logLevel: "info",
};

if (watch) {
  esbuild.context(options).then((ctx) => ctx.watch());
} else {
  esbuild.build(options);
}
