import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./index.css";
import { applyThemePreference, getStoredThemePreference } from "./lib/theme";

// Apply the saved/OS theme before first paint to avoid a flash of the wrong mode.
applyThemePreference(getStoredThemePreference());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
