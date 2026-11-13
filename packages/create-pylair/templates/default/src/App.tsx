import { useState } from "react";
import { call, stream } from "@pylair/client";

export default function App() {
  const [greeting, setGreeting] = useState("");
  const [nums, setNums] = useState<number[]>([]);

  return (
    <main style={{ fontFamily: "system-ui", padding: 32 }}>
      <h1>Pylair app</h1>
      <button onClick={async () => setGreeting(await call("hello", { name: "pylair" }))}>
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
      <button
        onClick={async () => {
          try {
            setGreeting("add(2,3) = " + String(await call("add", { a: 2, b: 3 })));
          } catch (e) {
            setGreeting("ERR: " + (e instanceof Error ? e.message : JSON.stringify(e)));
          }
        }}
      >
        add(2, 3)
      </button>
      <button
        onClick={async () => {
          try {
            setGreeting("add(x, 1) = " + String(await call("add", { a: "x", b: 1 })));
          } catch (e) {
            setGreeting("ERR: " + (e instanceof Error ? e.message : JSON.stringify(e)));
          }
        }}
      >
        add("x", 1)
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
