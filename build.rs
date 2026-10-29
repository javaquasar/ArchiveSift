fn main() {
    println!("cargo:rerun-if-changed=assets/icons/archive-sift.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icons/archive-sift.ico")
            .set("ProductName", "ArchiveSift")
            .set("FileDescription", "ArchiveSift")
            .set("OriginalFilename", "archive-sift.exe")
            .compile_for("archive-sift")
            .expect("Could not embed the ArchiveSift Windows icon");
    }
}
