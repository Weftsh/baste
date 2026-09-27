// Builds the npm packages for a release from its binary archives.
//
//   node npm/build.mjs v0.1.0 <dir with the release .tar.gz files> <out dir>
//
// Writes one package per platform (@weftsh/baste-<os>-<arch>, holding the
// binary) and the @weftsh/baste launcher that depends on them, then prints
// the package directories in publish order: platforms first.
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const [tag, releaseDir, outDir] = process.argv.slice(2);
if (!tag || !releaseDir || !outDir) {
  console.error("usage: node npm/build.mjs <tag> <release dir> <out dir>");
  process.exit(2);
}
if (!/^v\d+\.\d+\.\d+/.test(tag)) {
  console.error(`${tag} is not a version tag`);
  process.exit(2);
}
const version = tag.slice(1);

const launcher = JSON.parse(readFileSync(join(here, "baste/package.json"), "utf8"));
const license = join(here, "../LICENSE");

// Release target -> npm platform package. `files` are the binaries in the
// archive; the macOS one carries the Linux agent its VMs run.
const platforms = [
  { target: "x86_64-unknown-linux-musl", os: "linux", cpu: "x64", label: "Linux x64", files: ["baste"] },
  { target: "aarch64-unknown-linux-musl", os: "linux", cpu: "arm64", label: "Linux arm64", files: ["baste"] },
  { target: "aarch64-apple-darwin", os: "darwin", cpu: "arm64", label: "macOS on Apple Silicon", files: ["baste", "baste-linux-aarch64"] },
];

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });
const order = [];

for (const p of platforms) {
  const name = `@weftsh/baste-${p.os}-${p.cpu}`;
  const dir = join(outDir, `baste-${p.os}-${p.cpu}`);
  const archive = join(releaseDir, `baste-${tag}-${p.target}.tar.gz`);
  if (!existsSync(archive)) {
    console.error(`missing ${archive}`);
    process.exit(1);
  }
  mkdirSync(join(dir, "bin"), { recursive: true });
  execFileSync("tar", ["-xzf", resolve(archive), "-C", join(dir, "bin")]);
  for (const file of p.files) {
    const path = join(dir, "bin", file);
    if (!existsSync(path)) {
      console.error(`${archive} has no ${file}`);
      process.exit(1);
    }
    chmodSync(path, 0o755);
  }
  const pkg = {
    name,
    version,
    description: `The ${p.label} binary for @weftsh/baste. Install @weftsh/baste instead.`,
    homepage: launcher.homepage,
    repository: { ...launcher.repository, directory: "npm" },
    license: launcher.license,
    os: [p.os],
    cpu: [p.cpu],
    files: ["bin/"],
    preferUnplugged: true,
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  writeFileSync(
    join(dir, "README.md"),
    `# ${name}\n\nThe ${p.label} binary for [@weftsh/baste](https://www.npmjs.com/package/@weftsh/baste), local CI for GitHub Actions. Install that package instead; npm picks this one for you.\n`,
  );
  copyFileSync(license, join(dir, "LICENSE"));
  order.push(dir);
}

const main = join(outDir, "baste");
cpSync(join(here, "baste"), main, { recursive: true });
chmodSync(join(main, "bin/baste.js"), 0o755);
copyFileSync(license, join(main, "LICENSE"));
const pkg = { ...launcher, version, optionalDependencies: {} };
for (const p of platforms) {
  pkg.optionalDependencies[`@weftsh/baste-${p.os}-${p.cpu}`] = version;
}
writeFileSync(join(main, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
order.push(main);

console.log(order.join("\n"));
