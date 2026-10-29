//! On Windows, embeds a version resource in hatchzip.exe: the product name,
//! company, description and version that Explorer shows under Properties >
//! Details, and that code signing services and antivirus engines read; and
//! the application manifest, `hatchzip.manifest`. It depends on the target,
//! not the build machine, so this reads the target from Cargo instead of
//! `cfg`.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest = "hatchzip.manifest";
        println!("cargo:rerun-if-changed={manifest}");
        // The same names as hatchzip-gui's; winresource fills in the
        // version from the package. SignPath checks ProductName and
        // ProductVersion before it signs.
        winresource::WindowsResource::new()
            .set("ProductName", "HatchZip")
            .set("FileDescription", "HatchZip command line")
            .set("CompanyName", "HatchZip")
            .set("LegalCopyright", "Copyright (c) 2026 Yash Bavadiya")
            .set("OriginalFilename", "hatchzip.exe")
            .set("InternalName", "hatchzip")
            .set_manifest_file(manifest)
            .compile()
            .expect("embedding the Windows version resource and manifest");
    }
}
