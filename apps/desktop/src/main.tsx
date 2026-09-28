import React from "react";
import { createRoot } from "react-dom/client";
import "@fontsource/anek-latin/latin-400.css";
import "@fontsource/anek-latin/latin-500.css";
import "@fontsource/anek-latin/latin-600.css";
import "@fontsource/anek-latin/latin-700.css";
import "@fontsource/anek-latin/latin-ext-400.css";
import "./style.css";
import { WorkspaceHost } from "./Workspace";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <WorkspaceHost />
  </React.StrictMode>,
);
