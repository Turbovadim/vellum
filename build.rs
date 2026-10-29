fn main() {
    // Embed the app icon (resource #1, which GPUI also uses for the window and taskbar).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/vellum.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/vellum.ico")
            .set("FileDescription", "Vellum")
            .set("ProductName", "Vellum");
        res.compile().expect("failed to embed Windows resources");
    }
}
