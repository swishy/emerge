/// Compile-time check that the emerge_skia NIF init symbol is available.
///
/// The ERTS calls emerge_skia_nif_init() directly via --enable-static-nifs
/// during erts_start() — this file is NOT required for registration.
///
/// It serves as:
///   - A compile-time check that the symbol links (linker won't strip it)
///   - Documentation of the init function's signature
///
/// NOTE: A constructor-based approach would call emerge_skia_nif_init()
/// during dyld init (before erl_start), but Rustler's init function
/// depends on BEAM symbols (enif_*) via dlsym, which are only available
/// after the VM has started. The --enable-static-nifs mechanism ensures
/// the init function is called at the correct time during VM startup.

extern void emerge_skia_nif_init(void);
