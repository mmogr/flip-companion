fn main() {
    slint_build::compile("ui/main.slint").unwrap();

    // Export our custom `abort` symbol in the dynamic symbol table (.dynsym)
    // so that shared libraries (mesa's libgallium) resolve abort@plt to our
    // override instead of glibc's. This enables ELF symbol interposition.
    println!("cargo:rustc-link-arg=-Wl,--export-dynamic-symbol=abort");
}
