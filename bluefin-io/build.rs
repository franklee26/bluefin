use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        macos: { target_os = "macos" },
        linux: { target_os = "linux" },
        macos_fast: { all(target_os = "macos", feature = "macos-fast") },
    }
}
