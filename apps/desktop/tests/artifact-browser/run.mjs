import { createServer } from "vite";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

if (process.platform !== "darwin") throw new Error("This check requires macOS WebKit. Run npm test on other platforms.");
const root = fileURLToPath(new URL(".", import.meta.url));
const app = resolve(root, "../..");
const require = createRequire(join(app, "package.json"));
const scratch = await mkdtemp(join(tmpdir(), "medha-artifact-webkit-"));
let server;
try {
  server = await createServer({
    configFile: false, root, cacheDir: join(scratch, "vite"),
    resolve: { alias: ["react", "react/jsx-runtime", "react-dom/client", "react-dom"].map(name => ({ find: new RegExp(`^${name}$`), replacement: require.resolve(name) })) },
    esbuild: { jsx: "automatic" },
    plugins: [{ name: "isolated-native-context", enforce: "pre", resolveId(source, importer) {
      if (source === "./Workspace" && importer?.startsWith(join(app, "src"))) return "\0fixture-workspace";
    }, load(id) { if (id === "\0fixture-workspace") return "export const useWorkspace=()=>window.fixtureWorkspace; export const useWorkspaceApi=()=>window.fixtureWorkspace.api;"; } }],
    server: { host: "127.0.0.1", port: 0, fs: { allow: [app] } },
  });
  await server.listen();
  const address = server.httpServer.address();
  if (!address || typeof address === "string") throw new Error("The fixture did not listen.");
  await new Promise((done, reject) => {
    const child = spawn("swift", ["-module-cache-path", join(scratch, "swift"), join(root, "verify.swift"), `http://127.0.0.1:${address.port}/`], { stdio: "inherit" });
    const deadline = setTimeout(() => child.kill("SIGKILL"), 90_000);
    child.once("error", error => { clearTimeout(deadline); reject(error); });
    child.once("exit", (code, signal) => { clearTimeout(deadline); code === 0 ? done() : reject(new Error(`WebKit rendering check failed: ${code ?? signal}`)); });
  });
} finally {
  await server?.close();
  await rm(scratch, { recursive: true, force: true });
}
