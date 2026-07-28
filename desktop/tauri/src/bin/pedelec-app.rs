#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    pedelec_app_lib::pedelec_app::run();
}
