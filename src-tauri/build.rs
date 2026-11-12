fn main() {
    // Reads tauri.conf.json and capabilities/ at compile time and generates the glue code.
    tauri_build::build()
}
