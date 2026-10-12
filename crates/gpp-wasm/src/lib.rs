//! WebAssembly bindings used by the web page (`web/worker.js`).

use gpp_core::engine::Selection;
use wasm_bindgen::prelude::*;

/// All supported firmware definitions, as JSON.
#[wasm_bindgen]
pub fn catalog_json() -> String {
    serde_json::to_string(gpp_core::catalog()).unwrap()
}

/// `{ sha256, size, firmware }` for a file, as JSON (`firmware` is null if unsupported).
#[wasm_bindgen]
pub fn identify(data: &[u8]) -> String {
    serde_json::to_string(&gpp_core::identify(data)).unwrap()
}

#[wasm_bindgen]
pub struct Patched {
    data: Vec<u8>,
    report: String,
}

#[wasm_bindgen]
impl Patched {
    /// The patched file (moves it out; call once).
    pub fn take_data(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }

    /// The verification report, as JSON.
    pub fn report(&self) -> String {
        self.report.clone()
    }
}

/// Applies the patches in `selection_json` (`{ patches: [{ id, params }] }`) to the
/// stock file. `progress(stage, fraction)` is called along the way.
#[wasm_bindgen]
pub fn apply(data: &[u8], selection_json: &str, progress: &js_sys::Function) -> Result<Patched, JsError> {
    let sel: Selection = serde_json::from_str(selection_json)?;
    let mut report = |stage: &str, f: f32| {
        let _ = progress.call2(&JsValue::NULL, &JsValue::from_str(stage), &JsValue::from_f64(f as f64));
    };
    let out = gpp_core::apply(data, &sel, &mut report).map_err(|e| JsError::new(&e.0))?;
    Ok(Patched {
        report: serde_json::to_string(&out.report)?,
        data: out.data,
    })
}
