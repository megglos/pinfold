// Puts the icons, the manifest and the version data into the executable.
// Icon 1 is the application icon (Explorer shows the lowest id).
// Icon 2 is the tray icon during a pause.

fn main() {
    println!("cargo:rerun-if-changed=assets/pinfold.ico");
    println!("cargo:rerun-if-changed=assets/pinfold-paused.ico");
    println!("cargo:rerun-if-changed=assets/pinfold.manifest");
    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("assets/pinfold.manifest");
    res.set_icon_with_id("assets/pinfold.ico", "1");
    res.set_icon_with_id("assets/pinfold-paused.ico", "2");
    res.set("ProductName", "Pinfold");
    res.set("FileDescription", "Pinfold");
    res.compile().expect("failed to compile Windows resources");
}
