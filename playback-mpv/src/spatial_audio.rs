use std::{
    env,
    ffi::{CStr, c_char, c_void},
    path::{Path, PathBuf},
};

use libloading::Library;

const ORENDER_ABI_MAJOR: u32 = 0;

type VersionFn = unsafe extern "C" fn() -> u32;
type BuildIdFn = unsafe extern "C" fn() -> *const c_char;
type SpatialQueryFn = unsafe extern "C" fn(*const c_void) -> i32;

#[derive(Clone, Debug)]
pub(crate) struct OrenderProbe {
    pub path: PathBuf,
    pub minor: u32,
    pub build_id: Option<String>,
    pub spatial_query: &'static str,
}

pub(crate) fn probe_orender() -> Result<OrenderProbe, String> {
    if let Some(explicit) = env::var_os("ORENDER_LIBRARY").filter(|path| !path.is_empty()) {
        let path = PathBuf::from(explicit);
        return inspect_library(&path).map_err(|error| format!("{}: {error}", path.display()));
    }

    let mut failures = Vec::new();
    for candidate in library_candidates() {
        match inspect_library(&candidate) {
            Ok(probe) => return Ok(probe),
            Err(error) => failures.push(format!("{}: {error}", candidate.display())),
        }
    }
    Err(failures
        .into_iter()
        .next_back()
        .unwrap_or_else(|| "no liborender candidates were available".to_owned()))
}

fn inspect_library(path: &Path) -> Result<OrenderProbe, String> {
    // SAFETY: Loading executes the platform loader only. Every symbol is
    // validated before use and the library remains alive while symbols run.
    let library = unsafe { Library::new(path) }.map_err(|error| error.to_string())?;
    // SAFETY: The Omniphony ABI contract fixes both version signatures.
    let major = unsafe { library.get::<VersionFn>(b"orender_version_major\0") }
        .map_err(|_| "missing orender_version_major".to_owned())?;
    // SAFETY: The Omniphony ABI contract fixes both version signatures.
    let minor = unsafe { library.get::<VersionFn>(b"orender_version_minor\0") }
        .map_err(|_| "missing orender_version_minor".to_owned())?;
    // SAFETY: Both symbols were resolved from this live library with their
    // frozen no-argument ABI signatures.
    let (major, minor) = unsafe { (major(), minor()) };
    if major != ORENDER_ABI_MAJOR {
        return Err(format!(
            "unsupported ABI {major}.{minor}; expected major {ORENDER_ABI_MAJOR}"
        ));
    }

    // ABI minor versions are additive. Gate the object query on symbol
    // presence, preferring the 0.6 name and accepting its 0.5 alias.
    let spatial_query = if unsafe {
        library
            .get::<SpatialQueryFn>(b"orender_has_objects\0")
            .is_ok()
    } {
        "orender_has_objects"
    } else if unsafe {
        library
            .get::<SpatialQueryFn>(b"orender_is_spatial\0")
            .is_ok()
    } {
        "orender_is_spatial"
    } else {
        return Err("missing object/spatial query symbol".to_owned());
    };

    let build_id = unsafe { library.get::<BuildIdFn>(b"orender_build_id\0") }
        .ok()
        .and_then(|build_id| {
            // SAFETY: Optional build-id uses the documented static C-string
            // return value and the library remains loaded for this copy.
            let value = unsafe { build_id() };
            (!value.is_null()).then(|| {
                // SAFETY: A non-null build-id is documented as null-terminated.
                unsafe { CStr::from_ptr(value) }
                    .to_string_lossy()
                    .into_owned()
            })
        });

    Ok(OrenderProbe {
        path: path.to_path_buf(),
        minor,
        build_id,
        spatial_query,
    })
}

fn library_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_studio_candidate(&mut candidates);
    if let Some(candidate) = executable_library_candidate() {
        push_unique(&mut candidates, candidate);
    }
    for name in system_library_names() {
        push_unique(&mut candidates, PathBuf::from(name));
    }
    candidates
}

fn executable_library_candidate() -> Option<PathBuf> {
    env::current_exe()
        .ok()?
        .parent()
        .map(|directory| directory.join(platform_library_name()))
}

fn push_studio_candidate(candidates: &mut Vec<PathBuf>) {
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        push_unique(
            candidates,
            PathBuf::from(local_app_data)
                .join("omniphony")
                .join("lib")
                .join("orender.dll"),
        );
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = env::var_os("HOME") {
        push_unique(
            candidates,
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("omniphony")
                .join("lib")
                .join("liborender.dylib"),
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let data_home = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
        if let Some(data_home) = data_home {
            push_unique(
                candidates,
                data_home
                    .join("omniphony")
                    .join("lib")
                    .join("liborender.so.0"),
            );
        }
    }
}

fn push_unique(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

#[cfg(target_os = "windows")]
fn platform_library_name() -> &'static str {
    "orender.dll"
}

#[cfg(target_os = "macos")]
fn platform_library_name() -> &'static str {
    "liborender.dylib"
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_library_name() -> &'static str {
    "liborender.so.0"
}

#[cfg(target_os = "windows")]
fn system_library_names() -> &'static [&'static str] {
    &["orender.dll"]
}

#[cfg(target_os = "macos")]
fn system_library_names() -> &'static [&'static str] {
    &["liborender.dylib"]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_library_names() -> &'static [&'static str] {
    &["liborender.so.0", "liborender.so"]
}

#[cfg(test)]
mod tests {
    use super::{executable_library_candidate, library_candidates};

    #[test]
    fn candidates_should_include_liborender_beside_executable() {
        let expected = executable_library_candidate()
            .expect("the test executable should have a parent directory");

        assert!(
            library_candidates().contains(&expected),
            "missing executable-adjacent candidate: {}",
            expected.display()
        );
    }
}
