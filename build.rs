// Embeds the icons and version info into the executable.
// Icon id 1 is the application icon (lowest id, so Explorer shows it),
// id 2 is the tray icon while paused.

fn main() {
    println!("cargo:rerun-if-changed=assets/pinfold.ico");
    println!("cargo:rerun-if-changed=assets/pinfold-paused.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon_with_id("assets/pinfold.ico", "1");
    res.set_icon_with_id("assets/pinfold-paused.ico", "2");
    res.set("ProductName", "Pinfold");
    res.set("FileDescription", "Pinfold");
    res.compile().expect("failed to compile Windows resources");
}
