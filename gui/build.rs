//! Embeds the Aero icon into the Windows executable, so Explorer, the taskbar and shortcuts show it.

fn main() {
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
            let mut res = winres::WindowsResource::new();
            res.set_icon("assets/aero.ico");
            res.set("ProductName", "Aero Core");
            res.set("FileDescription", "Aero Core: node and wallet");
            if let Err(e) = res.compile() {
                // Not fatal: the app still runs, it just has the default executable icon.
                println!("cargo:warning=could not embed the Windows icon: {e}");
            }
        }
    }
    println!("cargo:rerun-if-changed=assets/aero.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
