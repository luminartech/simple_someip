fn main() {
    // Copy the images to the output when generating documentation
    println!("cargo:rerun-if-changed=assets/doc");
    std::fs::create_dir_all("target/doc").expect("Failed to create target/doc directory.");
    std::fs::copy("assets/doc/autosar_logo.svg", "target/doc/autosar_logo.svg")
        .expect("Failed to copy AUTOSAR logo when building documentation.");
}
