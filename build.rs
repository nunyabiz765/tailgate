fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("compile ui/app.slint");
    // Windows: the .exe's icon (Explorer, taskbar) and version info.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/tailgate.ico").set("ProductName", "Tailgate").set("FileDescription", "Tailgate");
        res.compile().expect("compile Windows resources");
    }
}
