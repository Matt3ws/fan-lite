// Embeds the UAC manifest. Fan control through PawnIO and NVML needs admin rights,
// so the release binary always asks for elevation. Set FANLITE_NO_UAC=1 to build an
// unelevated binary for development (hardware access then degrades gracefully).
fn main() {
    println!("cargo:rerun-if-env-changed=FANLITE_NO_UAC");
    println!("cargo:rerun-if-changed=third_party/pawnio");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !msvc {
        return;
    }
    let level = if std::env::var_os("FANLITE_NO_UAC").is_some() {
        "asInvoker"
    } else {
        "requireAdministrator"
    };
    println!("cargo:rustc-link-arg-bin=fan-lite=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=fan-lite=/MANIFESTUAC:level='{level}' uiAccess='false'");
}
