import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";

const bootEl = document.getElementById("boot-status");
if (bootEl) bootEl.remove();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
