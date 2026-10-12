//! Embeds every `firmware/*.toml` definition into the library.
use std::{env, fs, path::Path};

fn main() {
    let dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../firmware");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("firmware/ directory missing")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    let mut code = String::from("pub(crate) const DEFINITIONS: &[(&str, &str)] = &[\n");
    for f in files {
        println!("cargo:rerun-if-changed={}", f.display());
        let path = fs::canonicalize(&f).unwrap();
        let name = f.file_name().unwrap().to_string_lossy();
        code += &format!("    ({name:?}, include_str!({:?})),\n", path.display().to_string());
    }
    code += "];\n";
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("definitions.rs"), code).unwrap();
}
