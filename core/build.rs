// Generates the C# P/Invoke bindings from the extern "C" surface in src/lib.rs.
// One narrow ABI (init/send/free + one callback); the evolving command/event schema
// rides as JSON, so this file stays tiny and stable.
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");

    // The UI lives in a separate sibling repo (…-UI) that consumes the core as a black box, so we
    // emit the bindings into the core's own tree; sync-core.ps1 (UI side) copies this into ui/Interop.
    let out = "generated/NativeMethods.g.cs";
    if let Some(dir) = std::path::Path::new(out).parent() {
        std::fs::create_dir_all(dir).expect("create core/generated");
    }

    csbindgen::Builder::default()
        .input_extern_file("src/lib.rs")
        .csharp_dll_name("tronclass_core")
        .csharp_namespace("TronClass.Interop")
        .csharp_class_name("NativeMethods")
        .generate_csharp_file(out)
        .expect("csbindgen: generate C# bindings");
}
