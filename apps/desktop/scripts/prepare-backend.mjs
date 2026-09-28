import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const root = resolve(desktop, "../..");
const release = process.argv.includes("--release");
const profile = release ? "release" : "debug";
const target = execFileSync("rustc", ["--print", "host-tuple"], { encoding: "utf8" }).trim();
const suffix = process.platform === "win32" ? ".exe" : "";

execFileSync("cargo", ["build", "-p", "medha-cli", ...(release ? ["--release"] : [])], {
  cwd: root,
  stdio: "inherit",
});

const destination = join(desktop, "src-tauri", "binaries", `medha-${target}${suffix}`);
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(join(root, "target", profile, `medha${suffix}`), destination);
console.log(`Prepared Medha backend: ${destination}`);
