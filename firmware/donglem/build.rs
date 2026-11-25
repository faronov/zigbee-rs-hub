fn main() {
    embuild::espidf::sysenv::output();
    // The panic recorder (zrh-espapp's cpanic, shared with firmware/hub)
    // records the last ESP-IDF panic before ESP-IDF handles it.
    println!("cargo:rustc-link-arg=-Wl,--wrap=esp_panic_handler");
}
