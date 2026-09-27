// Fails when a page links to a local file that isn't in public/, or when the
// stylesheet wasn't built. Run after `npm run build`.
import { readFileSync, readdirSync, existsSync, statSync } from "node:fs";
import { join, dirname, relative } from "node:path";

const root = new URL("./public/", import.meta.url).pathname;
const pages = [];
const walk = (dir) => {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path);
    else if (name.endsWith(".html")) pages.push(path);
  }
};
walk(root);

const problems = [];
const css = join(root, "assets/site.css");
if (!existsSync(css) || statSync(css).size < 1000) problems.push("assets/site.css is missing; run npm run build");

for (const page of pages) {
  const html = readFileSync(page, "utf8");
  for (const [, url] of html.matchAll(/\s(?:href|src)="([^"]+)"/g)) {
    if (/^(https?:|mailto:|#|data:)/.test(url)) continue;
    let target = join(dirname(page), url.split(/[?#]/)[0]);
    if (url.endsWith("/") || url === "./" || url === "../") target = join(target, "index.html");
    if (!existsSync(target)) problems.push(`${relative(root, page)}: ${url} does not exist`);
  }
  for (const [, id] of html.matchAll(/\shref="#([^"]+)"/g)) {
    if (!html.includes(`id="${id}"`)) problems.push(`${relative(root, page)}: #${id} has no target`);
  }
}

if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(`${pages.length} pages OK`);
