//! Build a `ucorecfg` partition image from files, as the deployer does:
//!
//!   cargo run -p ucore --example bundle -- OUT.bin config.json [env.json]
//!
//! For testing a board or an emulator by hand; the deployer calls the same
//! `ucore::bundle::image`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, files @ ..] = args.as_slice() else {
        eprintln!("usage: bundle OUT.bin config.json [env.json]");
        std::process::exit(2);
    };
    let contents: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|f| {
            let name = std::path::Path::new(f).file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(f).unwrap_or_else(|e| panic!("{f}: {e}")))
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = contents.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
    let image = ucore::bundle::image(32 * 1024, &refs).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(out, image).unwrap_or_else(|e| panic!("{out}: {e}"));
    println!("{out}: {} with {}", 32 * 1024, contents.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
}
