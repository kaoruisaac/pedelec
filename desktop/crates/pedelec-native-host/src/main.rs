fn main() {
    if let Err(_err) = pedelec_native_host::run() {
        #[cfg(debug_assertions)]
        eprintln!("native messaging host failed: {_err}");
    }
}
