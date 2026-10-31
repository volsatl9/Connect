//! Keeps connect.exe free of libstdc++-6.dll when cross-compiled with mingw-w64.
//!
//! The hardware decoder's memory allocator (vk-mem, C++) asks to link libstdc++ dynamically,
//! which the static linking in `.cargo/config.toml` can't override. For a dynamic `-lstdc++`
//! the linker takes the first `libstdc++.dll.a` on its search path; this puts one there that is
//! really the static library, ahead of the import library that would need the DLL.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = env::var("TARGET").unwrap_or_default();
    if target != "x86_64-pc-windows-gnu" {
        return;
    }
    let cxx = env::var("CXX_x86_64_pc_windows_gnu").unwrap_or_else(|_| "x86_64-w64-mingw32-g++".into());
    println!("cargo:rerun-if-env-changed=CXX_x86_64_pc_windows_gnu");
    let output = Command::new(&cxx)
        .arg("-print-file-name=libstdc++.a")
        .output()
        .unwrap_or_else(|e| panic!("running {cxx} to find the static libstdc++: {e}"));
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert!(path.is_absolute() && path.exists(), "{cxx} has no static libstdc++ (got {path:?})");
    let dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("static-libstdc++");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(&path, dir.join("libstdc++.dll.a")).expect("copying the static libstdc++");
    println!("cargo:rustc-link-search=native={}", dir.display());
}
