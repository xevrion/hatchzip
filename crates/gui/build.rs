//! Compiles the `.slint` files with the built-in style that looks most at
//! home on the target system, and on Windows embeds the icon and version
//! into the executable. Both depend on the target, not the build machine,
//! so this reads the target from Cargo instead of `cfg`.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let style = match target_os.as_str() {
        "windows" => "fluent",
        "macos" => "cupertino",
        // Fluent is the most neutral of the rest next to GNOME and KDE, and
        // the only one besides Qt that ships a polished table and menus.
        _ => "fluent",
    };
    // SLINT_STYLE still wins, to preview another style without editing this.
    let style = std::env::var("SLINT_STYLE").unwrap_or_else(|_| style.to_string());
    println!("cargo:rerun-if-env-changed=SLINT_STYLE");
    let config = slint_build::CompilerConfiguration::new()
        .with_style(style)
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("compiling the UI");

    if target_os == "windows" {
        // Explorer, the Start menu, "Open with" and the right-click entries
        // all show icon 0 of the exe; the installer reads ProductVersion.
        let icon = "../../assets/icon/hatchzip.ico";
        println!("cargo:rerun-if-changed={icon}");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon)
            .set("ProductName", "HatchZip")
            .set("FileDescription", "HatchZip")
            .set("CompanyName", "HatchZip")
            .set("LegalCopyright", "MIT OR Apache-2.0");
        res.compile()
            .expect("embedding the Windows icon and version");
    }
}
