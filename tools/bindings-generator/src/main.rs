use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <input-lib-rs> <output-cs>", args[0]);
        std::process::exit(2);
    }
    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    if !input.is_file() {
        eprintln!("input not found: {}", input.display());
        std::process::exit(2);
    }
    // Ensure parent dir exists (caller uses system temp + same-dir staging).
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create output parent");
        }
    }
    csbindgen::Builder::default()
        .input_extern_file(input)
        .csharp_dll_name("tronclass_core")
        .csharp_namespace("TronClass.Interop")
        .csharp_class_name("NativeMethods")
        .generate_csharp_file(&output)
        .expect("csbindgen generate failed");
}
