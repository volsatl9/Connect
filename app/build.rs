//! Puts the app image and name on the Windows executable, where Explorer and the taskbar read them.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/connect.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/icon/connect.ico")
            .set("FileDescription", "Connect")
            .set("ProductName", "Connect")
            .compile()
            .expect("embedding the app image");
    }
}
