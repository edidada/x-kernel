import "../assets/fonts/fonts.css";
import { StrictMode } from "react";
import "./shared/i18n";
import { createRoot } from "react-dom/client";
import App from "./app/App";
import "./shared/styles/app.css";
import "./shared/styles/theme.css";

const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("Root element was not found");

createRoot(rootElement).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
