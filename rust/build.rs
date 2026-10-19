// Incrusta el icono y los datos de version en el .exe (Windows).
fn main() {
    #[cfg(windows)]
    {
        let mut recurso = winresource::WindowsResource::new();
        recurso
            .set_icon("assets/icon.ico")
            .set("ProductName", "viralshorts")
            .set("FileDescription", "viralshorts")
            .set("LegalCopyright", "viralshorts");
        if let Err(error) = recurso.compile() {
            println!("cargo:warning=no se pudo incrustar el icono: {error}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
