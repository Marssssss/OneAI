// esbuild bundler — bundles src/extension.ts → dist/extension.js (single file,
// no runtime node_modules needed in the .vsix). The webview HTML/JS is shipped
// as static assets (src/webview/*) and referenced via vscode-webview URIs.
const esbuild = require("esbuild");

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
