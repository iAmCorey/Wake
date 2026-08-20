fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=Wake.rc");

    #[cfg(windows)]
    {
        embed_resource::compile("Wake.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Wake Windows resources");
    }
}
