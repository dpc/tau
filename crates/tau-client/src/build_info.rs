//! Package-owned build metadata shared by core and standalone extensions.
//!
//! Packaging may stamp these fixed-width slots once per executable. Callers
//! supply their own build-script fallback, never the SDK's build identity.

#[cfg(test)]
mod tests;

/// Fixed-width source SHA slot shared with Tau's package stampers.
#[used]
#[allow(unsafe_code)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,.tau_build_info")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".tau_build_info"))]
static GIT_REVISION: [u8; 40] = *b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER____";

/// Fixed-width dirty-source slot shared with Tau's package stampers.
#[used]
#[allow(unsafe_code)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,.tau_build_info")
)]
#[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".tau_build_info"))]
static BUILD_STATE: [u8; 17] = *b"__TAU_BUILD_DIRTY";

/// Read package-patched bytes without compiler constant folding.
fn read_static<const N: usize>(value: &'static [u8; N]) -> [u8; N] {
    // SAFETY: this points to an initialized, aligned static array.
    #[allow(unsafe_code)]
    unsafe {
        core::ptr::read_volatile(value)
    }
}

/// Resolve package-stamped revision, falling back to the caller's Git metadata.
///
/// `git_revision` and `dirty` must come from the reporting package's build
/// script. Source archives without either source of metadata return `None`.
/// This does not read environment variables, files, or run subprocesses.
#[must_use]
pub fn revision(git_revision: Option<&str>, dirty: Option<bool>) -> Option<String> {
    resolve_revision(
        &read_static(&GIT_REVISION),
        &read_static(&BUILD_STATE),
        git_revision,
        dirty,
    )
}

/// Select the final executable's stamp or the reporting package's build
/// fallback.
fn resolve_revision(
    stamp: &[u8; 40],
    state: &[u8; 17],
    fallback: Option<&str>,
    dirty: Option<bool>,
) -> Option<String> {
    let (hash, modified) = if stamp.starts_with(b"__TAU_BUILD") {
        (fallback?, dirty == Some(true))
    } else {
        (
            std::str::from_utf8(stamp).ok()?,
            state.starts_with(b"modified"),
        )
    };
    if hash.is_empty() || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let short = &hash[..hash.len().min(7)];
    Some(if modified {
        format!("{short}-modified")
    } else {
        short.to_owned()
    })
}

/// Capture the caller's package release and build-script source metadata.
///
/// Prefer the no-argument form after calling `tau_client::build::emit()` from
/// the caller's build script, using the SDK's `build` feature as a
/// build-dependency. It verifies source ownership and installs Cargo watches.
///
/// The legacy module-argument form trusts caller-supplied `GIT_COMMIT_HASH` and
/// `GIT_DIRTY`. Plain `built::write_built_file()` does not establish ownership:
/// it can discover an enclosing consumer repository and miss Git-only changes.
#[macro_export]
macro_rules! package_build_identity {
    () => {
        $crate::BuildIdentity {
            release: env!("CARGO_PKG_VERSION").to_owned(),
            revision: $crate::build_info::revision(
                option_env!("TAU_PACKAGE_GIT_REVISION"),
                match option_env!("TAU_PACKAGE_GIT_DIRTY") {
                    Some("true") => Some(true),
                    Some("false") => Some(false),
                    _ => None,
                },
            ),
        }
    };
    ($built:ident) => {
        $crate::BuildIdentity {
            release: env!("CARGO_PKG_VERSION").to_owned(),
            revision: $crate::build_info::revision($built::GIT_COMMIT_HASH, $built::GIT_DIRTY),
        }
    };
}
