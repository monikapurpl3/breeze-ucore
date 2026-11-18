//! esp-hal's linker scripts, and a seed bundle for an emulated C6 (whose
//! flash QEMU cannot read back; see `store::open`): every file in
//! `UCORE_SEED_DIR`, embedded as it is.

use std::fmt::Write as _;

fn main() {
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    for var in ["UCORE_SEED_DIR", "UCORE_COMMIT"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let mut seed = String::from("pub static SEED: &[(&str, &[u8])] = &[\n");
    if let Some(dir) = std::env::var_os("UCORE_SEED_DIR") {
        println!("cargo:rerun-if-changed={}", std::path::Path::new(&dir).display());
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .expect("UCORE_SEED_DIR is a directory")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let path = e.path();
            println!("cargo:rerun-if-changed={}", path.display());
            let name = e.file_name().into_string().expect("an ASCII file name");
            writeln!(seed, "    ({name:?}, include_bytes!({:?})),", path.canonicalize().unwrap()).unwrap();
        }
    }
    seed.push_str("];\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("seed.rs");
    std::fs::write(out, seed).unwrap();
}
