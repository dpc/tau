//! Build metadata exposed to spawned processes (e.g. the shell
//! extension's child commands).
//!
//! Git revision and build date are stored as fixed-size byte arrays in
//! a dedicated link section. The Nix packaging step rewrites those
//! bytes in-place with `bbe` after the binary is built, which lets the
//! cargo build cache stay reproducible across commits.
//!
//! Why a `static [u8; N]` (and not the `built` crate's `&str`
//! constants): release builds with LTO inline short string literals as
//! `mov imm32` operands in `.text`, splitting the bytes across two
//! instructions in reverse order. The placeholder then no longer
//! exists as a contiguous byte sequence anywhere in the file, and bbe
//! has nothing to patch. A `#[used]` byte array in a named section (using
//! the target platform's section naming syntax) is guaranteed to live in
//! `.rodata`/data as one contiguous, unmerged blob.

mod built_info;

/// Magic prefix used to detect an unpatched binary (i.e. plain
/// `cargo build` outside of the Nix packaging path).
const PLACEHOLDER_TAG: &[u8] = b"__TAU_BUILD";

/// 40-byte slot for the git SHA-1 the binary was built from. Patched
/// in place by the Nix build's `bbe` post-processing.
#[used]
#[allow(unsafe_code)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,.tau_build_info")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".tau_build_info"))]
static GIT_REVISION: [u8; 40] = *b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER____";

/// 16-byte slot for the build date, formatted `YYYY-MM-DD HH:MM`.
/// Patched in place by the Nix build's `bbe` post-processing.
#[used]
#[allow(unsafe_code)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,.tau_build_info")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".tau_build_info"))]
static LAST_MODIFIED: [u8; 16] = *b"__TAU_BUILD_DATE";

/// Read a `static` byte array via `read_volatile` so the optimizer
/// can't fold the value back to its initializer (which would defeat
/// the bbe patch performed by the Nix build).
fn read_static<const N: usize>(s: &'static [u8; N]) -> [u8; N] {
    // Safety: `s` points to a static `[u8; N]` with a stable address;
    // a volatile read is always sound for a valid pointer.
    #[allow(unsafe_code)]
    unsafe {
        core::ptr::read_volatile(s)
    }
}

/// Git revision the harness was built from. Suffixed with
/// `-modified` when the working tree was dirty at build time.
#[must_use]
pub fn build_revision() -> String {
    let bytes = read_static(&GIT_REVISION);
    if bytes.starts_with(PLACEHOLDER_TAG) {
        return match (built_info::GIT_COMMIT_HASH_SHORT, built_info::GIT_DIRTY) {
            (Some(hash), Some(true)) => format!("{hash}-modified"),
            (Some(hash), _) => hash.to_owned(),
            _ => "unknown".to_owned(),
        };
    }
    let short = bytes.get(..7).unwrap_or(&bytes);
    std::str::from_utf8(short)
        .map(str::to_owned)
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// `YYYY-MM-DD HH:MM` of the build, with the Nix-packaging patch
/// taking precedence over the `built` timestamp. Returns `None` if no
/// usable value is available.
#[must_use]
pub fn build_last_modified() -> Option<String> {
    let bytes = read_static(&LAST_MODIFIED);
    if !bytes.starts_with(PLACEHOLDER_TAG)
        && let Ok(date) = std::str::from_utf8(&bytes)
    {
        return Some(date.to_owned());
    }
    short_built_time(built_info::BUILT_TIME_UTC).filter(|date| date != "1980-01-01 00:00")
}

fn short_built_time(_time: &str) -> Option<String> {
    // The `time` crate isn't a dependency of tau-harness; we surface
    // the raw RFC2822 string when no Nix override is available. Code
    // paths that need formatted output (like the CLI banner) read
    // their own `built::BUILT_TIME_UTC` and format it locally.
    None
}

/// Publish version metadata into the current process's environment so
/// children spawned by extensions (e.g. shell commands) inherit it.
/// Existing values are preserved — the env-var-only invocation path
/// (`tau component harness` launched without a parent CLI) still benefits
/// from values set externally, e.g. by integration tests.
pub fn export_to_env() {
    set_env_if_absent("TAU_VERSION", env!("CARGO_PKG_VERSION"));
    set_env_if_absent("TAU_BUILD", &build_revision());
    if let Some(date) = build_last_modified() {
        set_env_if_absent("TAU_LAST_MODIFIED", &date);
    }
}

fn set_env_if_absent(key: &str, value: &str) {
    if std::env::var_os(key).is_some() {
        return;
    }
    // Safety: called from `run_component` during single-threaded
    // startup before any extension subprocesses are spawned.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var(key, value);
    }
}
