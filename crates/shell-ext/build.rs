//! On Windows, embeds a version resource in hatchzip_shell_ext.dll, as for
//! the two programs: Explorer shows it under Properties > Details, and code
//! signing services and antivirus engines read it. It depends on the
//! target, not the build machine, so this reads the target from Cargo
//! instead of `cfg`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "HatchZip")
            .set("FileDescription", "HatchZip Explorer menu")
            .set("CompanyName", "HatchZip")
            .set("LegalCopyright", "MIT OR Apache-2.0")
            .set("OriginalFilename", "hatchzip_shell_ext.dll")
            .compile()
            .expect("embedding the Windows version resource");
    }
}
