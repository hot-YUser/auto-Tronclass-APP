// Generates the C# P/Invoke bindings from the extern "C" surface in src/lib.rs.
// One narrow ABI (init/send/free + one callback); the evolving command/event schema
// rides as JSON, so this file stays tiny and stable.
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");

    // The MAUI UI lives in ../ui (same repo). Emit the P/Invoke bindings straight into its Interop
    // folder; the file is committed so the UI compiles on MockCore without a core build first.
    let out = "../ui/Interop/NativeMethods.g.cs";
    if let Some(dir) = std::path::Path::new(out).parent() {
        std::fs::create_dir_all(dir).expect("create core/generated");
    }

    // Generate to a temp file first; only replace the tracked file when content differs.
    // This keeps `git status --porcelain` clean on rebuilds where the surface hasn't changed
    // and avoids spurious mtime touches that break exact-source guards.
    let tmp = std::env::var("OUT_DIR")
        .map(|d| format!("{d}/NativeMethods.g.cs.tmp"))
        .unwrap_or_else(|_| format!("{out}.tmp-{}", std::process::id()));
    csbindgen::Builder::default()
        .input_extern_file("src/lib.rs")
        .csharp_dll_name("tronclass_core")
        .csharp_namespace("TronClass.Interop")
        .csharp_class_name("NativeMethods")
        .generate_csharp_file(&tmp)
        .expect("csbindgen: generate C# bindings");

    let new_bytes = std::fs::read(&tmp).expect("read tmp bindings");
    let old_bytes = std::fs::read(out).unwrap_or_default();
    // Ensure staged tmp in repo root does not linger as untracked dirty.
    let _ = std::fs::remove_file(&tmp);
    if new_bytes != old_bytes {
        std::fs::write(out, &new_bytes).expect("write bindings");
    }
}
