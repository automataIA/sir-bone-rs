// Copies the browser bundles we render markdown/mermaid with into media/vendor,
// so the webview can load them locally (CSP forbids CDNs). Run by `npm run assets`.
import { mkdirSync, copyFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const out = join(root, "media", "vendor");
mkdirSync(out, { recursive: true });

const files = [
  ["marked/lib/marked.umd.js", "marked.umd.js"],
  ["dompurify/dist/purify.min.js", "purify.min.js"],
  ["mermaid/dist/mermaid.min.js", "mermaid.min.js"],
  ["@highlightjs/cdn-assets/highlight.min.js", "highlight.min.js"],
  ["@highlightjs/cdn-assets/styles/github-dark.min.css", "hljs-dark.css"],
  ["@highlightjs/cdn-assets/styles/github.min.css", "hljs-light.css"],
];

for (const [from, to] of files) {
  copyFileSync(join(root, "node_modules", from), join(out, to));
  console.log("copied", to);
}
