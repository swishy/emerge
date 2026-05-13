/// Constructor stub — ensures the emerge_skia NIF init symbol is reachable.
///
/// With --enable-static-nifs, OTP's ERTS init calls emerge_skia_nif_init()
/// directly during erts_start() — this file is NOT required for registration.
///
/// It serves as:
///   - A compile-time check that the symbol links (linker won't strip it)
///   - Documentation of the init function's signature
///   - A fallback if --enable-static-nifs is not used and the Rustler-internal
///     inventory constructors need reinforcement from a second call site
///
/// The `__attribute__((constructor))` fires before main(), during dyld init.
/// Calling emerge_skia_nif_init() twice (once here, once during erts_init) is
/// harmless — it just returns the same Nif pointer each time.

extern void emerge_skia_nif_init(void);

__attribute__((constructor))
static void emerge_skia_nif_register(void) {
    emerge_skia_nif_init();
}
