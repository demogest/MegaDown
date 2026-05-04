fn main() {
    slint_build::compile("ui/megadown.slint").expect("failed to compile Slint UI");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icons/icon.ico");
        resource
            .compile()
            .expect("failed to embed Windows application icon");
    }
}
