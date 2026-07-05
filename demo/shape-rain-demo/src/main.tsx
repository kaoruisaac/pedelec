import { render } from "solid-js/web";
import App from "./App";
import "./style.css";
import PopUpProvider from "./services/PopUpProvider";

const root = document.getElementById("root");

if (!root) {
  throw new Error("Root element not found");
}

render(() => <PopUpProvider><App /></PopUpProvider>, root);
