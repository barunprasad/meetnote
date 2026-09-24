import React from "react";
import ReactDOM from "react-dom/client";
// Bundled rather than fetched: meetnote never touches the network for its UI.
import "@fontsource-variable/figtree";
import App from "./App";
import Panel from "./Panel";

// One bundle, two windows. The backend opens the panel at `index.html#panel`,
// which is cheaper than shipping a second entry point and keeps the shared
// note-taking code in one place.
const Root = window.location.hash === "#panel" ? Panel : App;

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
