import { useState } from "react";
import { call, stream } from "@pylair/client";

export default function App() {
  const [greeting, setGreeting] = useState("");
  const [nums, setNums] = useState<number[]>([]);

  return (
    <main style={{ fontFamily: "system-ui", padding: 32 }}>
      <h1>Pylair app</h1>
      <button onClick={async () => setGreeting(await call("hello", { name: "tauri" }))}>
        Call python hello()
      </button>
      <button
        onClick={async () => {
          setNums([]);
          for await (const n of stream<number>("count", { n: 5 })) {
            setNums((xs) => [...xs, n as number]);
          }
        }}
      >
        Stream count(5)
      </button>
      <p>{greeting}</p>
      <p>{nums.join(" ")}</p>
      <p>
        <small>
          Run <code>pylair codegen</code> for typed stubs — the calls above use
          the raw client. Edit <code>backend/app.py</code> while
          <code> pylair dev</code> is running: the runtime restarts and stubs
          regenerate on save.
        </small>
      </p>
    </main>
  );
}
