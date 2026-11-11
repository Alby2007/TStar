import { createRoot } from "react-dom/client";
import { maybeSelfTest } from "@pylair/client";
import App from "./App";

createRoot(document.getElementById("root")!).render(<App />);
void maybeSelfTest();
