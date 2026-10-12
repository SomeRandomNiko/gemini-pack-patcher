// Runs the WebAssembly patcher off the main thread. The firmware file stays in
// this worker's memory; nothing is sent over the network.
import init, { catalog_json, identify, apply } from "./pkg/gpp_wasm.js";

const ready = init();
let input = null;

self.onmessage = async ({ data: msg }) => {
  await ready;
  try {
    if (msg.type === "catalog") {
      self.postMessage({ type: "catalog", catalog: JSON.parse(catalog_json()) });
    } else if (msg.type === "identify") {
      input = new Uint8Array(msg.buffer);
      self.postMessage({ type: "identified", result: JSON.parse(identify(input)) });
    } else if (msg.type === "apply") {
      const patched = apply(input, JSON.stringify(msg.selection), (stage, fraction) =>
        self.postMessage({ type: "progress", stage, fraction }),
      );
      const report = JSON.parse(patched.report());
      const out = patched.take_data();
      patched.free();
      self.postMessage({ type: "applied", report, buffer: out.buffer }, [out.buffer]);
    }
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
};
